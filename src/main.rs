use {
    chrono::prelude::*,
    clap::Arg,
    dioxus::prelude::*,
    rust_decimal::prelude::*,
    separator::FixedPlaceSeparatable,
    serde::{Deserialize, Serialize},
    solana_clap_utils::{input_parsers::*, input_validators::*},
    solana_client::rpc_client::RpcClient,
    solana_pubkey::Pubkey,
    solana_sdk::{
        account_utils::StateMut, message::Message, signers::Signers, system_program,
        transaction::Transaction,
    },
    std::{
        collections::{BTreeMap, BTreeSet, HashMap, HashSet},
        io::Write,
    },
    sys::{
        db::{self, *},
        exchange::*,
        notifier::*,
        priority_fee::PriorityFee,
        process::*,
        rpc_client_utils::get_signature_date,
        token::*,
        RpcClients,
    },
};

const FAVICON: Asset = asset!("/assets/favicon.ico");
const MAIN_CSS: Asset = asset!("/assets/main.css");
lazy_static::lazy_static! {
    static ref CONFIG: Config = {
        let home = std::env::var("HOME").unwrap_or_default();
        let home = std::path::PathBuf::from(home);
        let conf = home.join(".config").join("sys-ui").join("config.yml");
        let conf_clone = conf.clone();
        let file = std::fs::File::open(conf).unwrap_or_else(|e| {
            eprintln!(
                "Failed to open config file {}: {:?}",
                conf_clone.display(),
                e
            );
            std::process::exit(1);
        });
        serde_yaml::from_reader(file).unwrap_or_else(|e| {
            eprintln!(
                "Failed to read config from file {}: {:?}",
                conf_clone.display(),
                e
            );
            std::process::exit(1);
        })
    };

    static ref DB: std::sync::Arc<std::sync::RwLock<Db>> = {
        let db_path = std::path::PathBuf::from(CONFIG.db_path.clone());
        let mut db_fd_lock = fd_lock::RwLock::new(std::fs::File::open(&db_path).unwrap());
        let _db_write_lock = loop {
            match db_fd_lock.try_write() {
                Ok(lock) => break lock,
                Err(err) => {
                    eprintln!(
                        "Unable to lock database directory: {}: {}",
                        db_path.display(),
                        err
                    );
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        };
        let db = db::new(&db_path).unwrap_or_else(|err| {
            eprintln!("Failed to open {}: {}", db_path.display(), err);
            std::process::exit(1)
        });
        std::sync::Arc::new(std::sync::RwLock::new(db))
    };

    static ref RPC: std::sync::Arc<std::sync::RwLock<RpcClients>> =
        std::sync::Arc::new(
            std::sync::RwLock::new(
                RpcClients::new(CONFIG.json_rpc_url.clone(), None, None)));

}

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    pub db_path: String,
    pub json_rpc_url: String,
    pub authority_keypair: String,
}

#[derive(Clone)]
enum Sorting {
    Lot(bool),
    Date(bool),
    Amount(bool),
    Price(bool),
}

#[derive(Clone)]
enum DisposedSorting {
    Lot(bool),
    SaleDate(bool),
    AcqDate(bool),
    Amount(bool),
}

#[derive(Clone, Props)]
struct State {
    pub sorted: Option<Sorting>,
    pub amount: Option<f64>,
    pub authority: Option<String>,
    pub recipient: Option<String>,
    pub url: Option<String>,
    pub disposed_sorted: Option<DisposedSorting>,
}

impl PartialEq for State {
    fn eq(&self, _other: &State) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
struct GlobalState {
    state: Signal<State>,
    prices: Signal<BTreeMap<String, f64>>,
    account: Signal<Option<TrackedAccount>>,
    selected: Signal<BTreeSet<usize>>,
    disposed_selected: Signal<BTreeSet<usize>>,
    xaccount: Signal<Option<(Exchange, String)>>,
    xpmethod: Signal<Option<(Exchange, String)>>,
    xclients: Signal<Option<HashMap<Exchange, Box<dyn ExchangeClient>>>>,
    xupdate: Signal<bool>,
    reload: Signal<bool>,
    log: Signal<Option<String>>,
}

#[derive(Routable, Clone)]
enum Route {
    #[layout(NavBar)]
    #[route("/")]
    Main {},
    #[route("/disposed")]
    Disposed {},
    #[end_layout]
    #[route("/:..route")]
    PageNotFound { route: Vec<String> },
}

enum Action {}

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let db = DB.read().unwrap();
    let exchanges = db.get_exchanges();
    let mut xclients = HashMap::<_, _>::new();
    for x in exchanges {
        if let Some(credentials) = db.get_exchange_credentials(x, &"") {
            if let Ok(client) = exchange_client_new(x, credentials) {
                xclients.insert(x, client);
            }
        }
    }
    let selected = use_signal(|| BTreeSet::default());
    let disposed_selected = use_signal(|| BTreeSet::default());
    let log = use_signal(|| None);
    let _global_state = use_context_provider(|| GlobalState {
        state: Signal::new(State {
            sorted: None,
            amount: None,
            authority: Some(CONFIG.authority_keypair.clone()),
            recipient: None,
            url: Some(CONFIG.json_rpc_url.clone()),
            disposed_sorted: None,
        }),
        prices: Signal::new(BTreeMap::default()),
        account: Signal::new(None),
        selected,
        disposed_selected,
        xaccount: Signal::new(None),
        xpmethod: Signal::new(None),
        xclients: Signal::new(Some(xclients)),
        xupdate: Signal::new(false),
        reload: Signal::new(false),
        log,
    });

    let mut prices = use_context::<GlobalState>().prices;
    use_coroutine(move |_rx: UnboundedReceiver<Action>| async move {
        let mut tokens = vec![MaybeToken::from(None)];
        tokens.append(
            &mut Token::VARIANTS
                .into_iter()
                .map(|x| MaybeToken::from(Some(x)))
                .collect::<Vec<MaybeToken>>(),
        );
        loop {
            let rpc = RPC.read().unwrap();
            for token in tokens.iter() {
                let price = token
                    .get_current_price(&rpc.default())
                    .await
                    .map(|x| format!("{x:.6}").trim().parse::<f64>().unwrap())
                    .unwrap_or(0f64);
                prices.write().insert(token.to_string(), price);
            }
            drop(rpc);
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    rsx! {
        document::Link { rel: "icon", href: FAVICON }
        document::Link { rel: "stylesheet", href: MAIN_CSS }
        Router::<Route> {}
    }
}

#[component]
fn NavBar() -> Element {
    rsx! {
        nav {
            Link { to: Route::Main {}, "Holdings" }
            Link { to: Route::Disposed {}, "Disposed" }
        }
        Outlet::<Route> {}
    }
}

#[component]
pub fn Main() -> Element {
    rsx! {
        Menu {}
        div { id: "sys",
            Accounts {}
            Lots {}
            Tokens {}
        }
        Input {}
        Summary {}
        Log {}
    }
}

#[component]
pub fn Menu() -> Element {
    let account = use_context::<GlobalState>().account.read().clone();
    let address = account.map(|x| x.address);
    let sync = move |_| {
        spawn(async move {
            let mut buffer = std::io::BufWriter::new(Vec::new());
            match process_account_sync(
                &mut DB.write().unwrap(),
                &RPC.read().unwrap(),
                address,
                None,
                false,
                false,
                &Notifier::default(),
                &mut buffer,
            )
            .await
            {
                Ok(()) => {
                    let bytes = buffer.into_inner().unwrap();
                    consume_context::<GlobalState>()
                        .log
                        .set(Some(String::from_utf8(bytes).unwrap()));
                }
                Err(e) => consume_context::<GlobalState>().log.set(Some(format!(
                    "Failed sys account sync {:?}: {:?}",
                    address, e
                ))),
            }
            consume_context::<GlobalState>().reload.set(true);
            consume_context::<GlobalState>().xupdate.set(true);
        });
    };
    let mut account = use_context::<GlobalState>().account;
    let mut state = use_context::<GlobalState>().state;
    let split = move |_| {
        spawn(async move {
            do_split(&mut account, &mut state).await;
            *(use_context::<GlobalState>().reload.write()) = true;
        });
    };
    let deactivate = move |_| {
        spawn(async move { do_deactivate(&mut account, &state).await });
    };
    let withdraw = move |_| {
        spawn(async move {
            do_withdraw(&mut account, &mut state).await;
            *(use_context::<GlobalState>().reload.write()) = true;
            *(use_context::<GlobalState>().xupdate.write()) = true;
        });
    };
    let delegate = move |_| {
        spawn(async move { do_delegate(&mut account, &state).await });
    };
    let swap = move |_| {
        spawn(async move {
            do_swap(&mut account, &mut state).await;
            *(use_context::<GlobalState>().reload.write()) = true;
        });
    };
    let merge = move |_| {
        spawn(async move {
            do_merge(&mut account, &state).await;
            *(use_context::<GlobalState>().reload.write()) = true;
        });
    };
    let xclients = use_context::<GlobalState>().xclients;
    let disburse = move |_| {
        let xaccount = use_context::<GlobalState>().xaccount.read().clone();
        let xpmethod = use_context::<GlobalState>().xpmethod.read().clone();
        spawn(async move {
            do_disburse(xaccount, xpmethod, &xclients, &state).await;
            *(use_context::<GlobalState>().xupdate.write()) = true;
        });
    };
    let url = state.read().url.clone().unwrap_or_default();
    rsx! {
        div { id: "menu",
            button { onclick: sync, "Sync" }
            button { onclick: split, "Split" }
            button { onclick: deactivate, "Deactivate" }
            button { onclick: withdraw, "Withdraw" }
            button { onclick: delegate, "Delegate" }
            button { onclick: swap, "Swap" }
            button { onclick: merge, "Merge" }
            button { onclick: disburse, "Disburse" }
            label { r#for: "json_rpc_url", "url:" }
            input {
                id: "json_rpc_url",
                name: "json_rpc_url",
                value: url,
                oninput: move |event| {
                    let mut state = state.write();
                    let value = event.value();
                    state.url = Some(value.clone());
                    let mut rpc = RPC.write().unwrap();
                    *rpc = RpcClients::new(value, None, None);
                },
            }
        }
    }
}

#[component]
pub fn Accounts() -> Element {
    rsx! {
        div { id: "accounts",
            AccountsList {}
            AccountState {}
            Exchanges {}
        }
    }
}

#[component]
pub fn AccountsList() -> Element {
    if *(use_context::<GlobalState>().reload.read()) {
        println!("Reloading accounts");
    }
    let accounts = DB.read().unwrap().get_accounts();

    rsx! {
        div { id: "account_list",
            ul {
                for account in accounts {
                    AccountItem { account: account.clone() }
                }
            }
        }
    }
}

#[component]
pub fn AccountState() -> Element {
    let account = use_context::<GlobalState>().account.read().clone();
    let rpc = RPC.read().unwrap();

    if let Some(account) = account {
        let content = account.description.to_string();
        let mut buffer = std::io::BufWriter::new(Vec::new());
        if get_account_state(&rpc, account.token, account.address, &mut buffer).is_ok() {
            let bytes = buffer.into_inner().unwrap();
            let account_state = String::from_utf8(bytes).unwrap();
            rsx! {
                div { id: "account_state",
                    pre {
                        "{content}"
                        br {}
                        "{account_state}"
                    }
                }
            }
        } else {
            rsx! {
                div { id: "account_state", "{content}" }
            }
        }
    } else {
        rsx! {
            div { id: "account_state" }
        }
    }
}

#[component]
pub fn Exchanges() -> Element {
    let exchanges = DB.read().unwrap().get_exchanges();

    rsx! {
        div { id: "exchanges",
            ul {
                for exchange in exchanges {
                    ExchangeItem { exchange: exchange.clone() }
                }
            }
        }
    }
}

#[component]
fn ExchangeItem(exchange: Exchange) -> Element {
    rsx! {
        "{exchange}"
        ExchangeAccounts { exchange: exchange.clone() }
        PaymentMethods { exchange: exchange.clone() }
    }
}

#[component]
fn ExchangeAccounts(exchange: Exchange) -> Element {
    let xclients = use_context::<GlobalState>().xclients;
    let xupdate = use_context::<GlobalState>().xupdate;
    let accounts = use_resource(move || async move {
        if *xupdate.read() {
            println!("Fetch data from exchange");
        }
        let xclients = xclients.read();
        let client = xclients.as_ref().unwrap().get(&exchange).unwrap();
        client.accounts().await
    });
    let accs = accounts.read();
    if accs.is_none() {
        return rsx! {
            div { "Accounts" }
        };
    }
    let accs = accs.as_ref().unwrap();
    if accs.is_err() {
        return rsx! {
            div { "Accounts" }
        };
    }
    rsx! {
        div { "Accounts" }
        div {
            ul {
                for acc in accs.as_ref().unwrap() {
                    if acc.value.parse::<f64>().unwrap() > 0. || acc.currency == "USDC" {
                        ExchangeAccountsItem { exchange: exchange.clone(), account: acc.clone() }
                    }
                }
            }
        }
    }
}

#[component]
fn ExchangeAccountsItem(exchange: Exchange, account: AccountInfo) -> Element {
    let xclients = use_context::<GlobalState>().xclients;
    let xaccount = use_context::<GlobalState>().xaccount.read().clone();
    let mut kind = "regular";
    if let Some((selected_exchange, selected_account)) = xaccount {
        if exchange == selected_exchange && account.uuid == selected_account {
            kind = "selected";
        }
    }
    let currency = account.currency.clone();
    let token = Token::from_str(&currency);
    let resource = use_resource(move || async move {
        let xclients = xclients.read();
        let client = xclients.as_ref().unwrap().get(&exchange).unwrap();
        client.deposit_address(MaybeToken::from(token.ok())).await
    });

    rsx! {
        li {
            class: kind,
            onclick: move |event| {
                let modifiers = event.data().modifiers();
                let is_meta = modifiers.meta() || (modifiers.alt() && modifiers.ctrl());
                let address = if currency == "SOL" || token.is_ok() {
                    let address = resource.read();
                    address.as_ref().map(|x| x.as_ref().map(|x| x.to_string()).ok()).flatten()
                } else {
                    None
                };
                let mut xaccount = use_context::<GlobalState>().xaccount;
                let mut state = use_context::<GlobalState>().state;
                (*xaccount.write(), state.write().recipient) = if is_meta && kind == "selected" {
                    (None, None)
                } else {
                    (Some((exchange, account.uuid.clone())), address.clone())
                };
            },
            "{account.name} {account.currency} {account.value}"
        }
    }
}

#[component]
fn PaymentMethods(exchange: Exchange) -> Element {
    let xclients = use_context::<GlobalState>().xclients;
    let payment_methods = use_resource(move || async move {
        let xclients = xclients.read();
        let client = xclients.as_ref().unwrap().get(&exchange).unwrap();
        client.payment_methods().await
    });
    let methods = payment_methods.read();
    if methods.is_none() {
        return rsx! {
            div { "Payment methods" }
        };
    }
    let methods = methods.as_ref().unwrap();
    if methods.is_err() {
        return rsx! {
            div { "Payment methods" }
        };
    }
    rsx! {
        div { "Payment methods" }
        div {
            ul {
                for method in methods.as_ref().unwrap() {
                    PaymentMethodsItem { exchange: exchange.clone(), method: method.clone() }
                }
            }
        }
    }
}

#[component]
fn PaymentMethodsItem(exchange: Exchange, method: PaymentInfo) -> Element {
    let xpmethod = use_context::<GlobalState>().xpmethod.read().clone();
    let mut kind = "regular";
    if let Some((selected_exchange, selected_method)) = xpmethod {
        if exchange == selected_exchange && method.id == selected_method {
            kind = "selected";
        }
    }
    rsx! {
        li {
            class: kind,
            onclick: move |event| {
                let modifiers = event.data().modifiers();
                let is_meta = modifiers.meta() || (modifiers.alt() && modifiers.ctrl());
                let mut selected_method = use_context::<GlobalState>().xpmethod;
                *selected_method.write() = if is_meta && kind == "selected" {
                    None
                } else {
                    Some((exchange, method.id.clone()))
                };
            },
            "{method.name} {method.r#type} {method.currency}"
        }
    }
}

#[component]
pub fn Lots() -> Element {
    let mut state = use_context::<GlobalState>().state;
    let account = use_context::<GlobalState>().account.read().clone();
    let prices = use_context::<GlobalState>().prices.read().clone();

    if let Some(account) = account {
        let price = *prices.get(&account.token.to_string()).unwrap_or(&0f64);
        let mut lots = account.lots;
        if let Some(ref sorting) = state.read().sorted {
            match *sorting {
                Sorting::Lot(d) => {
                    lots.sort_by(|a, b| {
                        if d {
                            a.lot_number.cmp(&b.lot_number)
                        } else {
                            b.lot_number.cmp(&a.lot_number)
                        }
                    });
                }
                Sorting::Date(d) => {
                    lots.sort_by(|a, b| {
                        if d {
                            a.acquisition.when.cmp(&b.acquisition.when)
                        } else {
                            b.acquisition.when.cmp(&a.acquisition.when)
                        }
                    });
                }
                Sorting::Amount(d) => {
                    lots.sort_by(|a, b| {
                        if d {
                            a.amount.cmp(&b.amount)
                        } else {
                            b.amount.cmp(&a.amount)
                        }
                    });
                }
                Sorting::Price(d) => {
                    lots.sort_by(|a, b| {
                        if d {
                            a.acquisition.price().cmp(&b.acquisition.price())
                        } else {
                            b.acquisition.price().cmp(&a.acquisition.price())
                        }
                    });
                }
            }
        }
        rsx! {
            div { id: "lots",
                table {
                    tr {
                        th {
                            id: "lot_number",
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Lot(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Lot(v));
                            },
                            "Lot"
                        }
                        th {
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Date(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Date(v));
                            },
                            "Date"
                        }
                        th {
                            id: "lot_amount",
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Amount(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Amount(v));
                            },
                            "Amount"
                        }
                        th {
                            id: "lot_price",
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Price(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Price(v));
                            },
                            "Price"
                        }
                        th { id: "lot_term", "Term" }
                        th { "Gain" }
                    }
                    for lot in lots {
                        LotItem {
                            token: account.token,
                            lot: lot.clone(),
                            price,
                        }
                    }
                }
            }
        }
    } else {
        rsx! {
            div { id: "lots" }
        }
    }
}

#[component]
fn AccountItem(account: TrackedAccount) -> Element {
    let mut state = use_context::<GlobalState>().state;
    let selected_account = use_context::<GlobalState>().account.read().clone();
    let address = format!(
        "{:?} ({}) {}",
        account.address,
        account.token.name(),
        account.token.ui_amount(account.last_update_balance),
    );
    let mut kind = "regular";
    if let Some(selected_account) = selected_account {
        if account.address == selected_account.address && account.token == selected_account.token {
            kind = "selected";
        }
    }
    rsx! {
        li {
            class: kind,
            onclick: move |event| {
                let modifiers = event.data().modifiers();
                let mut state = state.write();
                if modifiers == Modifiers::ALT {
                    state.recipient = Some(account.address.to_string());
                    return;
                }
                let is_meta = modifiers.meta() || (modifiers.alt() && modifiers.ctrl());
                if is_meta || kind == "regular" {
                    use_context::<GlobalState>().selected.write().clear();
                }
                let mut selected_account = use_context::<GlobalState>().account;
                *selected_account.write() = if is_meta && kind == "selected" {
                    None
                } else {
                    Some(account.clone())
                };
            },
            "{address}"
        }
    }
}

#[component]
fn LotItem(token: MaybeToken, lot: Lot, price: f64) -> Element {
    let mut selected = use_context::<GlobalState>().selected;
    let lot_number = format!("{}", lot.lot_number);
    let lot_amount = token.format_amount(lot.amount).to_string();
    let lot_date = format!("{}", lot.acquisition.when);
    let lot_price = format!(
        "${}",
        f64::try_from(lot.acquisition.price())
            .unwrap()
            .separated_string_with_fixed_place(2)
    );
    let today = chrono::Local::now().date_naive();
    let term = if today.signed_duration_since(lot.acquisition.when).num_days() < 365 {
        "S"
    } else {
        "L"
    };
    let gain = format!(
        "${}",
        (token.ui_amount(lot.amount) * (price - f64::try_from(lot.acquisition.price()).unwrap()))
            .separated_string_with_fixed_place(2)
    );
    let kind = if selected.read().contains(&lot.lot_number) {
        "selected"
    } else {
        "regular"
    };
    let account = use_context::<GlobalState>().account.read().clone();
    let sorted = use_context::<GlobalState>().state.read().sorted.clone();
    let select_lot = move |event: Event<MouseData>| {
        let lot = lot.lot_number;
        let mut selected = selected.write();
        let modifiers = event.data().modifiers();
        if modifiers.shift() {
            if let Some(ref account) = account {
                let mut lots = account.lots.clone();
                if let Some(sorting) = sorted.clone() {
                    match sorting {
                        Sorting::Lot(d) => {
                            lots.sort_by(|a, b| {
                                if d {
                                    a.lot_number.cmp(&b.lot_number)
                                } else {
                                    b.lot_number.cmp(&a.lot_number)
                                }
                            });
                        }
                        Sorting::Date(d) => {
                            lots.sort_by(|a, b| {
                                if d {
                                    a.acquisition.when.cmp(&b.acquisition.when)
                                } else {
                                    b.acquisition.when.cmp(&a.acquisition.when)
                                }
                            });
                        }
                        Sorting::Amount(d) => {
                            lots.sort_by(|a, b| {
                                if d {
                                    a.amount.cmp(&b.amount)
                                } else {
                                    b.amount.cmp(&a.amount)
                                }
                            });
                        }
                        Sorting::Price(d) => {
                            lots.sort_by(|a, b| {
                                if d {
                                    a.acquisition.price().cmp(&b.acquisition.price())
                                } else {
                                    b.acquisition.price().cmp(&a.acquisition.price())
                                }
                            });
                        }
                    }
                }
                let mut sel_end = 0;
                let mut sel_beg = 0;
                for (i, l) in lots.iter().enumerate() {
                    if l.lot_number == lot {
                        sel_end = i;
                    } else if selected.contains(&l.lot_number) {
                        sel_beg = i;
                    }
                }
                if sel_beg > sel_end {
                    std::mem::swap(&mut sel_beg, &mut sel_end);
                }
                for i in sel_beg..=sel_end {
                    selected.insert(lots[i].lot_number);
                }
            }
        } else if modifiers.meta() {
            if selected.contains(&lot) {
                selected.remove(&lot);
            } else {
                selected.insert(lot);
            }
        } else {
            selected.clear();
            selected.insert(lot);
        }
    };
    rsx! {
        tr {
            class: kind,
            onclick: select_lot,
            td { class: "lot_number", "{lot_number}" }
            td { class: "lot_date", "{lot_date}" }
            td { class: "lot_amount", "{lot_amount}" }
            td { class: "lot_price", "{lot_price}" }
            td { class: "lot_term", "{term}" }
            td { "{gain}" }
        }
    }
}

#[component]
fn Tokens() -> Element {
    let prices = use_context::<GlobalState>().prices.read().clone();
    rsx! {
        div { id: "tokens",
            table {
                for (token , price) in prices.into_iter() {
                    tr {
                        class: "token",
                        onclick: move |_| {
                            let mut state = use_context::<GlobalState>().state;
                            state.write().recipient = Some(token.to_string());
                        },
                        td { class: "token", "{token}" }
                        td { class: "token", "${price}" }
                    }
                }
            }
        }
    }
}

#[component]
pub fn Input() -> Element {
    let mut state = use_context::<GlobalState>().state;
    let authority = state.read().authority.clone().unwrap_or_default();
    let recipient = state.read().recipient.clone().unwrap_or_default();
    let amount = state.read().amount.clone().unwrap_or_default();
    rsx! {
        div { id: "authority",
            label { r#for: "authority", "authority:" }
            input {
                id: "authority",
                name: "authority",
                value: authority,
                oninput: move |event| state.write().authority = Some(event.value()),
            }
            label { r#for: "recipient", "recipient:" }
            input {
                id: "recipient",
                name: "recipient",
                value: recipient,
                oninput: move |event| {
                    let value = event.value();
                    state.write().recipient = if value.is_empty() { None } else { Some(value) };
                },
            }
            label { r#for: "amount", "amount:" }
            input {
                id: "amount",
                name: "amount",
                value: amount,
                oninput: move |event| {
                    let value = event.value();
                    if !value.ends_with(".") && !(value.contains(".") && value.ends_with("0")) {
                        state.write().amount = value.parse::<f64>().ok();
                    }
                },
            }
        }
    }
}

#[component]
pub fn Summary() -> Element {
    let selected_account = use_context::<GlobalState>().account.read().clone();
    let prices = use_context::<GlobalState>().prices.read().clone();
    let selected = use_context::<GlobalState>().selected;
    let db = DB.read().unwrap();
    let (long_term_gain_tax_rate, short_term_gain_tax_rate) =
        if let Some(ref rate) = db.get_tax_rate() {
            (rate.long_term_gain, rate.short_term_gain)
        } else {
            (0.22f64, 0.3935f64)
        };
    let accounts = db.get_accounts();
    let mut held_tokens = BTreeMap::<MaybeToken, u64>::default();
    for account in accounts {
        if let std::collections::btree_map::Entry::Vacant(e) = held_tokens.entry(account.token) {
            e.insert(0);
        }
        let held_token = held_tokens.get_mut(&account.token).unwrap();
        *held_token += account.last_update_balance;
    }
    let mut total = 0f64;
    let mut selected_price = 0f64;
    for (t, a) in held_tokens.clone() {
        let price = *prices.get(&t.to_string()).unwrap_or(&0f64);
        if let Some(ref account) = selected_account {
            if t == account.token {
                selected_price = price;
            }
        }
        total += price * t.ui_amount(a);
    }
    let mut summary = format!("total ${} (", total.separated_string_with_fixed_place(2));
    for (i, (t, a)) in held_tokens.iter().enumerate() {
        if i == 0 {
            summary = format!("{summary}{}", t.format_amount(*a));
        } else {
            summary = format!("{summary} {}", t.format_amount(*a));
        }
    }
    summary = format!("{summary})");
    if !selected.read().is_empty() {
        if let Some(account) = selected_account {
            let selected_lots_value = account
                .lots
                .iter()
                .filter(|x| selected.read().contains(&x.lot_number))
                .fold(0u64, |acc, x| acc + x.amount);
            let cost = account
                .lots
                .iter()
                .filter(|x| selected.read().contains(&x.lot_number))
                .fold(0f64, |acc, x| {
                    acc + x.acquisition.price().to_f64().unwrap()
                        * account.token.ui_amount(x.amount)
                });
            let today = chrono::Local::now().date_naive();
            let (short_gain, long_gain) = account
                .lots
                .iter()
                .filter(|x| selected.read().contains(&x.lot_number))
                .fold((0f64, 0f64), |acc, x| {
                    let amount = account.token.ui_amount(x.amount);
                    let basis = amount * x.acquisition.price().to_f64().unwrap();
                    let value = amount * selected_price;
                    if today.signed_duration_since(x.acquisition.when).num_days() < 365 {
                        (acc.0 + value - basis, acc.1)
                    } else {
                        (acc.0, acc.1 + value - basis)
                    }
                });
            let value = account.token.ui_amount(selected_lots_value) * selected_price;
            let gain = short_gain + long_gain;
            summary = format!(
                "{summary}, selected lots value {} = ${}, cost ${}, gain ${}",
                account.token.format_amount(selected_lots_value),
                value.separated_string_with_fixed_place(2),
                cost.separated_string_with_fixed_place(2),
                gain.separated_string_with_fixed_place(2),
            );
            if gain > 0f64 {
                let tax = if short_gain > 0f64 && long_gain > 0f64 {
                    short_gain * short_term_gain_tax_rate + long_gain * long_term_gain_tax_rate
                } else if long_gain > 0f64 {
                    (short_gain + long_gain) * long_term_gain_tax_rate
                } else {
                    (short_gain + long_gain) * short_term_gain_tax_rate
                };
                summary = format!(
                    "{summary}, tax ${}",
                    tax.separated_string_with_fixed_place(2),
                );
            }
        }
    }
    rsx! {
        div { id: "summary", "{summary}" }
    }
}

#[component]
pub fn Log() -> Element {
    let log = use_context::<GlobalState>().log.read().clone();
    if let Some(content) = log {
        rsx! {
            div { id: "log",
                pre { "{content}" }
            }
        }
    } else {
        rsx! {
            div { id: "log" }
        }
    }
}

#[component]
pub fn Disposed() -> Element {
    let mut disposed_lots = DB.read().unwrap().disposed_lots().clone();
    let mut state = use_context::<GlobalState>().state;

    if let Some(ref sorting) = state.read().disposed_sorted {
        match *sorting {
            DisposedSorting::Lot(d) => {
                disposed_lots.sort_by(|a, b| {
                    if d {
                        a.lot.lot_number.cmp(&b.lot.lot_number)
                    } else {
                        b.lot.lot_number.cmp(&a.lot.lot_number)
                    }
                });
            }
            DisposedSorting::SaleDate(d) => {
                disposed_lots.sort_by(|a, b| {
                    if d {
                        a.when.cmp(&b.when)
                    } else {
                        b.when.cmp(&a.when)
                    }
                });
            }
            DisposedSorting::AcqDate(d) => {
                disposed_lots.sort_by(|a, b| {
                    if d {
                        a.lot.acquisition.when.cmp(&b.lot.acquisition.when)
                    } else {
                        b.lot.acquisition.when.cmp(&a.lot.acquisition.when)
                    }
                });
            }
            DisposedSorting::Amount(d) => {
                disposed_lots.sort_by(|a, b| {
                    if d {
                        a.lot.amount.cmp(&b.lot.amount)
                    } else {
                        b.lot.amount.cmp(&a.lot.amount)
                    }
                });
            }
        }
    }

    rsx! {
        div { id: "disposed",
            table {
                thead {
                    tr {
                        th {
                            onclick: move |_| {
                                let sorted = state.read().disposed_sorted.clone();
                                let mut v = true;
                                if let Some(DisposedSorting::Lot(x)) = sorted {
                                    v = !x;
                                }
                                state.write().disposed_sorted = Some(DisposedSorting::Lot(v));
                            },
                            "Lot"
                        }
                        th {
                            onclick: move |_| {
                                let sorted = state.read().disposed_sorted.clone();
                                let mut v = true;
                                if let Some(DisposedSorting::SaleDate(x)) = sorted {
                                    v = !x;
                                }
                                state.write().disposed_sorted = Some(DisposedSorting::SaleDate(v));
                            },
                            "Sale Date"
                        }
                        th {
                            onclick: move |_| {
                                let sorted = state.read().disposed_sorted.clone();
                                let mut v = true;
                                if let Some(DisposedSorting::AcqDate(x)) = sorted {
                                    v = !x;
                                }
                                state.write().disposed_sorted = Some(DisposedSorting::AcqDate(v));
                            },
                            "Acq Date"
                        }
                        th {
                            onclick: move |_| {
                                let sorted = state.read().disposed_sorted.clone();
                                let mut v = true;
                                if let Some(DisposedSorting::Amount(x)) = sorted {
                                    v = !x;
                                }
                                state.write().disposed_sorted = Some(DisposedSorting::Amount(v));
                            },
                            "Amount"
                        }
                        th { "Income" }
                        th { "Sale Price" }
                        th { "Acq Price" }
                        th { "Cap Gain" }
                        th { "Term" }
                    }
                }
                tbody {
                    for lot in disposed_lots {
                        DisposedLotItem { lot: lot.clone() }
                    }
                }
            }
        }
        DisposedSummary {}
    }
}

#[component]
fn DisposedLotItem(lot: DisposedLot) -> Element {
    let lot_number = format!("{}", lot.lot.lot_number);
    let amount = lot.token.format_amount(lot.lot.amount).to_string();
    let acq_date = lot.lot.acquisition.when.to_string();
    let acq_price = f64::try_from(lot.lot.acquisition.price()).unwrap();
    let sale_date = lot.when.to_string();
    let sale_price = f64::try_from(lot.price()).unwrap();
    let income =
        if let LotAcquistionKind::EpochReward { epoch: _, slot: _ } = lot.lot.acquisition.kind {
            acq_price * lot.token.ui_amount(lot.lot.amount)
        } else {
            0.0
        };
    let gain = format!(
        "{}",
        (lot.token.ui_amount(lot.lot.amount) * (sale_price - acq_price))
            .separated_string_with_fixed_place(2)
    );
    let acq_price = format!("${}", acq_price.separated_string_with_fixed_place(2));
    let sale_price = format!("${}", sale_price.separated_string_with_fixed_place(2));
    let income = format!("${}", income.separated_string_with_fixed_place(2));
    let term = if lot
        .when
        .signed_duration_since(lot.lot.acquisition.when)
        .num_days()
        < 365
    {
        "S"
    } else {
        "L"
    };

    let mut selected = use_context::<GlobalState>().disposed_selected;
    let kind = if selected.read().contains(&lot.lot.lot_number) {
        "selected"
    } else {
        "regular"
    };
    let sorted = use_context::<GlobalState>()
        .state
        .read()
        .disposed_sorted
        .clone();

    let select_lot = move |event: Event<MouseData>| {
        let lot = lot.lot.lot_number;
        let mut selected = selected.write();
        let modifiers = event.data().modifiers();
        if modifiers.shift() {
            let mut disposed_lots = DB.read().unwrap().disposed_lots().clone();
            if let Some(sorting) = sorted.clone() {
                match sorting {
                    DisposedSorting::Lot(d) => {
                        disposed_lots.sort_by(|a, b| {
                            if d {
                                a.lot.lot_number.cmp(&b.lot.lot_number)
                            } else {
                                b.lot.lot_number.cmp(&a.lot.lot_number)
                            }
                        });
                    }
                    DisposedSorting::SaleDate(d) => {
                        disposed_lots.sort_by(|a, b| {
                            if d {
                                a.when.cmp(&b.when)
                            } else {
                                b.when.cmp(&a.when)
                            }
                        });
                    }
                    DisposedSorting::AcqDate(d) => {
                        disposed_lots.sort_by(|a, b| {
                            if d {
                                a.lot.acquisition.when.cmp(&b.lot.acquisition.when)
                            } else {
                                b.lot.acquisition.when.cmp(&a.lot.acquisition.when)
                            }
                        });
                    }
                    DisposedSorting::Amount(d) => {
                        disposed_lots.sort_by(|a, b| {
                            if d {
                                a.lot.amount.cmp(&b.lot.amount)
                            } else {
                                b.lot.amount.cmp(&a.lot.amount)
                            }
                        });
                    }
                }
            }
            let mut sel_end = 0;
            let mut sel_beg = 0;
            for (i, l) in disposed_lots.iter().enumerate() {
                if l.lot.lot_number == lot {
                    sel_end = i;
                } else if selected.contains(&l.lot.lot_number) {
                    sel_beg = i;
                }
            }
            if sel_beg > sel_end {
                std::mem::swap(&mut sel_beg, &mut sel_end);
            }
            for i in sel_beg..=sel_end {
                selected.insert(disposed_lots[i].lot.lot_number);
            }
        } else if modifiers.meta() {
            if selected.contains(&lot) {
                selected.remove(&lot);
            } else {
                selected.insert(lot);
            }
        } else {
            selected.clear();
            selected.insert(lot);
        }
    };

    rsx! {
        tr {
            class: kind,
            onclick: select_lot,
            td { class: "lot_number", "{lot_number}" }
            td { class: "lot_date", "{sale_date}" }
            td { class: "lot_date", "{acq_date}" }
            td { class: "lot_amount", "{amount}" }
            td { "{income}" }
            td { "{sale_price}" }
            td { "{acq_price}" }
            td { "{gain}" }
            td { class: "lot_term", "{term}" }
        }
    }
}

#[component]
pub fn DisposedSummary() -> Element {
    fn aggregate(lots: &Vec<DisposedLot>, p: impl Fn(&usize) -> bool) -> (u64, f64, f64, f64, f64) {
        lots.iter()
            .filter(|x| p(&x.lot.lot_number))
            .fold((0u64, 0f64, 0f64, 0f64, 0f64), |acc, x| {
                let amount = x.token.ui_amount(x.lot.amount);
                let basis = amount * x.lot.acquisition.price().to_f64().unwrap();
                let value = amount * x.price().to_f64().unwrap();
                (
                    if x.token.is_sol() {
                        acc.0 + x.lot.amount
                    } else {
                        acc.0
                    },
                    acc.1 + amount * f64::try_from(x.price()).unwrap(),
                    if let LotAcquistionKind::EpochReward { epoch: _, slot: _ } =
                        &x.lot.acquisition.kind
                    {
                        acc.2 + amount * f64::try_from(x.lot.acquisition.price()).unwrap()
                    } else {
                        acc.2
                    },
                    if x.when
                        .signed_duration_since(x.lot.acquisition.when)
                        .num_days()
                        < 365
                    {
                        acc.3 + value - basis
                    } else {
                        acc.3
                    },
                    if x.when
                        .signed_duration_since(x.lot.acquisition.when)
                        .num_days()
                        < 365
                    {
                        acc.4
                    } else {
                        acc.4 + value - basis
                    },
                )
            })
    }
    let disposed = DB.read().unwrap().disposed_lots();
    let selected = use_context::<GlobalState>().disposed_selected;
    let (amount, value, income, short_gain, long_gain) = aggregate(&disposed, |_| true);
    let mut summary = format!(
        "Total disposed lots {} tokens {} value ${} income ${} short-term gain ${} long-term gain ${}",
        disposed.len(),
        MaybeToken::SOL().format_amount(amount),
        value.separated_string_with_fixed_place(2),
        income.separated_string_with_fixed_place(2),
        short_gain.separated_string_with_fixed_place(2),
        long_gain.separated_string_with_fixed_place(2),
    );
    if !selected.read().is_empty() {
        let (amount, value, income, short_gain, long_gain) =
            aggregate(&disposed, |a| selected.read().contains(a));
        summary = format!(
            "{}\n      selected lots {} tokens {} value ${} income ${} short-term gain ${} long-term gain ${}",
            summary,
            selected.read().len(),
            MaybeToken::SOL().format_amount(amount),
            value.separated_string_with_fixed_place(2),
            income.separated_string_with_fixed_place(2),
            short_gain.separated_string_with_fixed_place(2),
            long_gain.separated_string_with_fixed_place(2),
        );
    }
    rsx! {
        pre {"{summary}"}
    }
}

#[component]
fn PageNotFound(route: Vec<String>) -> Element {
    rsx! {
        h1 { "Page not found" }
        p { "We are terribly sorry, but the page you requested doesn't exist." }
        pre { color: "red", "log:\nattemped to navigate to: {route:?}" }
    }
}

macro_rules! make_arg_matches {
    {$name:expr, $value:ident, $func:ident} => {
        clap::App::new("sys")
            .arg(
                Arg::with_name($name)
                    .long($name)
                    .takes_value(true)
                    .validator($func),
            )
            .get_matches_from(vec!["sys", &format!("--{}", $name), &$value])
    }
}

macro_rules! make_signer {
    {$signer:ident, $log:ident} => {
        {
            let arg_matches = make_arg_matches!("by", $signer, is_valid_signer);
            let mut wallet_manager = None;
            let (signer, address) = match signer_of(&arg_matches, "by", &mut wallet_manager) {
                Ok(v) => v,
                Err(e) => {
                    *$log.write() = Some(format!("Invalid signer {}: {:?}", $signer, e));
                    return;
                }
            };
            (signer.expect("signer"), address.expect("address"))
        }
    }
}

async fn do_split(
    selected_account: &mut Signal<Option<TrackedAccount>>,
    state: &mut Signal<State>,
) {
    let mut log = use_context::<GlobalState>().log;
    let mut selected = use_context::<GlobalState>().selected;
    *log.write() = None;
    if selected.read().is_empty() || selected_account.read().is_none() {
        *log.write() = Some("Select account and lots to split".to_string());
        return;
    }
    if state.read().authority.is_none() {
        *log.write() = Some("Enter staking authority keypair for account to be split".to_string());
        return;
    }
    let rpc = RPC.read().unwrap();
    let mut db = DB.write().unwrap();
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let amount = account
        .lots
        .iter()
        .filter(|x| selected.read().contains(&x.lot_number))
        .fold(0, |acc, x| acc + x.amount);
    let description = None;
    let lot_selection_method = LotSelectionMethod::default();
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| selected.read().contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let authority = state.read().authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, log);
    let recipient = state.read().recipient.clone();
    let to_keypair = recipient.map(|r| {
        let arg_matches = make_arg_matches!("to", r, is_keypair);
        keypair_of(&arg_matches, "to").unwrap()
    });
    let if_balance_exceeds = None;
    let priority_fee = PriorityFee::default_auto();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_account_split(
        &mut db,
        &rpc,
        from_address,
        Some(amount),
        description,
        lot_selection_method,
        Some(lot_numbers),
        authority_address,
        vec![authority_signer],
        to_keypair,
        if_balance_exceeds,
        priority_fee,
        &mut buffer,
    )
    .await
    {
        *log.write() = Some(format!(
            "Failed sys account split {:?} {}: {:?}",
            account.address,
            account.token.format_amount(amount),
            e,
        ));
        return;
    }
    adjust_balance(&mut db, authority_address);
    *selected_account.write() = None;
    selected.write().clear();
    let bytes = buffer.into_inner().unwrap();
    *log.write() = Some(String::from_utf8(bytes).unwrap());
}

async fn do_deactivate(
    selected_account: &mut Signal<Option<TrackedAccount>>,
    state: &Signal<State>,
) {
    let mut log = use_context::<GlobalState>().log;
    let state = state.read();
    *log.write() = None;
    if selected_account.read().is_none() {
        *log.write() = Some("Select account to deactivate".to_string());
        return;
    }
    if state.authority.is_none() {
        *log.write() =
            Some("Enter staking authority keypair for account to be deactivated".to_string());
        return;
    }
    let rpc = RPC.read().unwrap();
    let account = selected_account.read().clone().unwrap();
    let authority = state.authority.clone().unwrap();
    *log.write() = Some(format!(
        "deactivate-stake --stake-authority {} {}\nCheck ledger device for signing",
        authority, account.address,
    ));
    let (authority_signer, authority_address) = make_signer!(authority, log);
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_stake_deactivate(
        &rpc,
        account.address,
        authority_address,
        vec![authority_signer],
        &mut buffer,
    )
    .await
    {
        *log.write() = Some(format!(
            "Failed solana deactivate-stake --stake-authority {:?} {:?}: {:?}",
            authority, account.address, e,
        ));
    }
    let mut db = DB.write().unwrap();
    adjust_balance(&mut db, authority_address);
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    *log.write() = Some(String::from_utf8(bytes).unwrap());
}

async fn do_withdraw(
    selected_account: &mut Signal<Option<TrackedAccount>>,
    state: &mut Signal<State>,
) {
    let mut log = use_context::<GlobalState>().log;
    let mut selected = use_context::<GlobalState>().selected;
    *log.write() = None;
    if selected.read().is_empty() || selected_account.read().is_none() {
        *log.write() = Some("Select account and lots to withdraw".to_string());
        return;
    }
    if state.read().recipient.is_none() {
        *log.write() = Some("Enter account address to deposit the withdrawn funds to".to_string());
        return;
    }
    if state.read().authority.is_none() {
        *log.write() =
            Some("Enter withdraw authority keypair for account to withdraw from".to_string());
        return;
    }
    let rpc = RPC.read().unwrap();
    let mut db = DB.write().unwrap();
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let amount = if state.read().amount.unwrap_or_default() > 0. {
        account.token.amount(state.read().amount.unwrap())
    } else {
        account
            .lots
            .iter()
            .filter(|x| selected.read().contains(&x.lot_number))
            .fold(0, |acc, x| acc + x.amount)
    };
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| selected.read().contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let lot_selection_method = LotSelectionMethod::default();
    let recipient = state.read().recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let to_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            *log.write() = Some(format!("Invalid address to deposit to {}", recipient));
            return;
        }
    };
    let authority = state.read().authority.clone().unwrap();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if !account.token.is_sol() {
        if let Err(e) = process_token_transfer(
            &state.read().url.clone().unwrap(),
            &authority,
            &account.token.mint().to_string(),
            &format!("{}", account.token.ui_amount(amount)),
            &state.read().recipient.clone().unwrap(),
            &mut buffer,
        )
        .await
        {
            *log.write() = Some(format!(
                "Failed spl-token transfer --owner {:?} {}: {:?}",
                account.address,
                account.token.format_amount(amount),
                e,
            ));
            return;
        }
        let (_, authority_address) = make_signer!(authority, log);
        adjust_balance(&mut db, authority_address);
        *selected_account.write() = None;
        selected.write().clear();
        if let Err(e) = db.record_drop(
            account.address,
            account.token,
            amount,
            LotSelectionMethod::LastInFirstOut,
            Some(lot_numbers),
        ) {
            *log.write() = Some(format!("Failed to drop lots: {e:#?}"));
        }
        let bytes = buffer.into_inner().unwrap();
        *log.write() = Some(String::from_utf8(bytes).unwrap());
        return;
    }
    let custodian = None;
    let (authority_signer, authority_address) = make_signer!(authority, log);
    if let Err(e) = process_stake_withdraw(
        &mut db,
        &rpc,
        from_address,
        authority_address,
        to_address,
        custodian,
        Some(amount),
        lot_selection_method,
        Some(lot_numbers),
        vec![authority_signer],
        &mut buffer,
    )
    .await
    {
        *log.write() = Some(format!(
            "Failed solana withdraw-stake {:?} {}: {:?}",
            account.address,
            account.token.format_amount(amount),
            e,
        ));
        return;
    }
    adjust_balance(&mut db, authority_address);
    *selected_account.write() = None;
    selected.write().clear();
    let bytes = buffer.into_inner().unwrap();
    *log.write() = Some(String::from_utf8(bytes).unwrap());
}

async fn do_delegate(selected_account: &mut Signal<Option<TrackedAccount>>, state: &Signal<State>) {
    let mut log = use_context::<GlobalState>().log;
    let state = state.read();
    *log.write() = None;
    if selected_account.read().is_none() {
        *log.write() = Some("Select account to delegate".to_string());
        return;
    }
    if state.recipient.is_none() {
        *log.write() = Some("Enter validator address to delegate to".to_string());
        return;
    }
    if state.authority.is_none() {
        *log.write() = Some("Enter staking authority keypair for account to delegate".to_string());
        return;
    }
    let rpc = RPC.read().unwrap();
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let recipient = state.recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let to_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            *log.write() = Some(format!("Invalid validator address {}", recipient));
            return;
        }
    };
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, log);
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_stake_delegate(
        &rpc,
        from_address,
        authority_address,
        to_address,
        vec![authority_signer],
        &mut buffer,
    )
    .await
    {
        *log.write() = Some(format!(
            "Failed solana delegate-stake --stake-authority {} {} {}: {:?}",
            authority, from_address, to_address, e,
        ));
    }
    let mut db = DB.write().unwrap();
    adjust_balance(&mut db, authority_address);
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    *log.write() = Some(String::from_utf8(bytes).unwrap());
}

async fn do_swap(selected_account: &mut Signal<Option<TrackedAccount>>, state: &mut Signal<State>) {
    let mut selected = use_context::<GlobalState>().selected;
    let state = state.read();
    consume_context::<GlobalState>().log.set(None);
    if selected.read().is_empty() || selected_account.read().is_none() {
        consume_context::<GlobalState>()
            .log
            .set(Some("Select account and lots to swap".to_string()));
        return;
    }
    if state.authority.is_none() {
        consume_context::<GlobalState>()
            .log
            .set(Some("Enter signer keypair for swap".to_string()));
        return;
    }
    let rpc = RPC.read().unwrap();
    let mut db = DB.write().unwrap();
    let account = selected_account.read().clone().unwrap();
    let authority = state.authority.clone().unwrap();
    let mut log = use_context::<GlobalState>().log;
    let (signer, address) = make_signer!(authority, log);
    let from_token = account.token;
    let recipient = state.recipient.clone().unwrap_or_default();
    let to_token = MaybeToken::from(
        if sys::token::is_valid_token_or_sol(recipient.clone()).is_ok() {
            Token::from_str(&recipient).unwrap()
        } else {
            Token::USDC
        },
    );
    let amount = account
        .lots
        .iter()
        .filter(|x| selected.read().contains(&x.lot_number))
        .fold(0, |acc, x| acc + x.amount);
    let ui_amount = Some(from_token.ui_amount(amount));
    let slippage_bps = 100u64;
    let lot_selection_method = LotSelectionMethod::LastInFirstOut;
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| selected.read().contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let signature = None;
    let if_from_balance_exceeds = None;
    let for_no_less_than = None;
    let max_coingecko_value_percentage_loss = 5f64;
    let priority_fee = PriorityFee::default_auto();
    let notifier = Notifier::default();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    match process_jup_swap(
        &mut db,
        &rpc,
        address,
        from_token,
        to_token,
        ui_amount,
        slippage_bps,
        lot_selection_method,
        Some(lot_numbers),
        vec![signer],
        signature,
        if_from_balance_exceeds,
        for_no_less_than,
        max_coingecko_value_percentage_loss,
        priority_fee,
        &notifier,
        &mut buffer,
    )
    .await
    {
        Ok(()) => match process_sync_swaps(&mut db, rpc.default(), &notifier, &mut buffer).await {
            Ok(()) => {
                adjust_balance(&mut db, address);
                *selected_account.write() = None;
                selected.write().clear();
                let bytes = buffer.into_inner().unwrap();
                consume_context::<GlobalState>()
                    .log
                    .set(Some(String::from_utf8(bytes).unwrap()))
            }
            Err(e) => consume_context::<GlobalState>()
                .log
                .set(Some(format!("Failed sync swaps: {:?}", e,))),
        },
        Err(e) => consume_context::<GlobalState>().log.set(Some(format!(
            "Failed sys jup swap {:?} {} {} {}: {:?}",
            authority,
            from_token,
            to_token,
            from_token.ui_amount(amount),
            e,
        ))),
    }
}

async fn do_merge(selected_account: &mut Signal<Option<TrackedAccount>>, state: &Signal<State>) {
    let mut log = use_context::<GlobalState>().log;
    let state = state.read();
    *log.write() = None;
    if selected_account.read().is_none() {
        *log.write() = Some("Select account to merge".to_string());
        return;
    }
    if state.recipient.is_none() {
        *log.write() = Some("Enter account address to be merged into".to_string());
        return;
    }
    if state.authority.is_none() {
        *log.write() = Some("Enter staking authority keypair for account to be merged".to_string());
        return;
    }
    let rpc = RPC.read().unwrap();
    let mut db = DB.write().unwrap();
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let recipient = state.recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let into_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            *log.write() = Some(format!("Invalid address to merge into {}", recipient));
            return;
        }
    };
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, log);
    let priority_fee = PriorityFee::default_auto();
    let signature = None;
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_account_merge(
        &mut db,
        &rpc,
        from_address,
        into_address,
        authority_address,
        vec![authority_signer],
        priority_fee,
        signature,
        &mut buffer,
    )
    .await
    {
        *log.write() = Some(format!(
            "Failed sys account merge {:?} --into {:?}: {:?}",
            from_address, into_address, e,
        ));
        return;
    }
    adjust_balance(&mut db, authority_address);
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    *log.write() = Some(String::from_utf8(bytes).unwrap());
}

async fn do_disburse(
    xaccount: Option<(Exchange, String)>,
    xpmethod: Option<(Exchange, String)>,
    xclients: &Signal<Option<HashMap<Exchange, Box<dyn ExchangeClient>>>>,
    state: &Signal<State>,
) {
    let mut log = use_context::<GlobalState>().log;
    let state = state.read();
    let xclients = xclients.read();
    if xaccount.is_none() {
        *log.write() = Some("Select exchange account from which to disburse cash".to_string());
        return;
    }
    if xpmethod.is_none() {
        *log.write() = Some("Select bank account to which to disburse cash".to_string());
        return;
    }
    let (exchange, account) = xaccount.unwrap();
    let client = xclients.as_ref().unwrap().get(&exchange).unwrap();
    let amount = if state.amount.clone().unwrap_or_default() > 0. {
        state.amount.clone().unwrap_or_default().to_string()
    } else {
        let accounts = client.accounts().await;
        if let Err(e) = accounts {
            *log.write() = Some(format!("Couldn't get exchange accounts {e}"));
            return;
        }
        accounts
            .unwrap()
            .iter()
            .find(|x| x.uuid == account)
            .map(|x| x.value.clone())
            .unwrap()
    };
    let methods = client.payment_methods().await;
    if let Err(e) = methods {
        *log.write() = Some(format!("Couldn't get exchange payment methods {e}"));
        return;
    }
    let method = xpmethod.unwrap().1;
    let currency = methods
        .unwrap()
        .iter()
        .find(|x| x.id == method.clone())
        .map(|x| x.currency.clone())
        .unwrap();
    let disbursement = client
        .disburse_cash(account, amount, currency, method)
        .await;
    match disbursement {
        Ok(d) => {
            *log.write() = Some(format!(
                "Disbursed cash ${}, fee ${}, reference {} {:#?}",
                d.total, d.total_fee, d.user_reference, d.user_warnings,
            ))
        }
        Err(e) => *log.write() = Some(format!("{e}")),
    }
}

pub async fn process_stake_deactivate<T: Signers, W: Write>(
    rpc_clients: &RpcClients,
    stake_account: Pubkey,
    stake_authority: Pubkey,
    signers: T,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let rpc_client = rpc_clients.default();
    let (recent_blockhash, last_valid_block_height) =
        rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment())?;
    let instructions = vec![solana_sdk::stake::instruction::deactivate_stake(
        &stake_account,
        &stake_authority,
    )];
    let message = Message::new(&instructions, Some(&stake_authority));
    let mut transaction = Transaction::new_unsigned(message);
    transaction.message.recent_blockhash = recent_blockhash;
    let simulation_result = rpc_client.simulate_transaction(&transaction)?.value;
    if simulation_result.err.is_some() {
        return Err(format!("Simulation failure: {simulation_result:?}").into());
    }
    transaction.try_sign(&signers, recent_blockhash)?;
    let signature = transaction.signatures[0];
    writeln!(writer, "Transaction signature: {signature}")?;

    if !sys::send_transaction_until_expired(rpc_clients, &transaction, last_valid_block_height)
        .unwrap_or_default()
    {
        return Err("Deactivate failed".into());
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn process_stake_withdraw<T: Signers, W: Write>(
    db: &mut Db,
    rpc_clients: &RpcClients,
    stake_address: Pubkey,
    stake_authority: Pubkey,
    to_address: Pubkey,
    custodian: Option<&Pubkey>,
    amount: Option<u64>,
    lot_selection_method: LotSelectionMethod,
    lot_numbers: Option<HashSet<usize>>,
    signers: T,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let rpc_client = rpc_clients.default();
    let (recent_blockhash, last_valid_block_height) =
        rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment())?;
    let token = MaybeToken::SOL();
    let stake_account = db
        .get_account(stake_address, MaybeToken::SOL())
        .ok_or_else(|| format!("SOL account does not exist for {stake_address}"))?;
    if db.get_account(to_address, MaybeToken::SOL()).is_none() {
        return Err(format!("Account {} ({}) does not exist", to_address, token,).into());
    }
    let (withdraw_all, amount) = match amount {
        None => (true, stake_account.last_update_balance),
        Some(amount) => (false, amount),
    };
    let instructions = vec![solana_sdk::stake::instruction::withdraw(
        &stake_address,
        &stake_authority,
        &to_address,
        amount,
        custodian,
    )];
    let message = Message::new(&instructions, Some(&stake_authority));
    let mut transaction = Transaction::new_unsigned(message);
    transaction.message.recent_blockhash = recent_blockhash;
    let simulation_result = rpc_client.simulate_transaction(&transaction)?.value;
    if simulation_result.err.is_some() {
        return Err(format!("Simulation failure: {simulation_result:?}").into());
    }
    transaction.try_sign(&signers, recent_blockhash)?;
    let signature = transaction.signatures[0];
    writeln!(writer, "Transaction signature: {signature}")?;
    db.record_transfer(
        signature,
        last_valid_block_height,
        Some(amount),
        stake_address,
        token,
        to_address,
        token,
        lot_selection_method,
        lot_numbers,
    )?;
    if !sys::send_transaction_until_expired(rpc_clients, &transaction, last_valid_block_height)
        .unwrap_or_default()
    {
        db.cancel_transfer(signature)?;
        return Err("Withdraw failed".into());
    }
    writeln!(writer, "Withdraw confirmed: {signature}")?;
    let when = get_signature_date(rpc_client, signature).await?;
    db.confirm_transfer(signature, when)?;
    if withdraw_all {
        let stake_account = db.get_account(stake_address, MaybeToken::SOL()).unwrap();
        assert!(stake_account.lots.is_empty());
        db.remove_account(stake_address, MaybeToken::SOL())?;
    }
    Ok(())
}

pub async fn process_stake_delegate<T: Signers, W: Write>(
    rpc_clients: &RpcClients,
    stake_address: Pubkey,
    stake_authority: Pubkey,
    to_address: Pubkey,
    signers: T,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let rpc_client = rpc_clients.default();
    let (recent_blockhash, last_valid_block_height) =
        rpc_client.get_latest_blockhash_with_commitment(rpc_client.commitment())?;
    let instructions = vec![solana_sdk::stake::instruction::delegate_stake(
        &stake_address,
        &stake_authority,
        &to_address,
    )];
    let message = Message::new(&instructions, Some(&stake_authority));
    let mut transaction = Transaction::new_unsigned(message);
    transaction.message.recent_blockhash = recent_blockhash;
    let simulation_result = rpc_client.simulate_transaction(&transaction)?.value;
    if simulation_result.err.is_some() {
        return Err(format!("Simulation failure: {simulation_result:?}").into());
    }
    transaction.try_sign(&signers, recent_blockhash)?;
    let signature = transaction.signatures[0];
    writeln!(writer, "Transaction signature: {signature}")?;
    if !sys::send_transaction_until_expired(rpc_clients, &transaction, last_valid_block_height)
        .unwrap_or_default()
    {
        return Err("Delegate failed".into());
    }
    Ok(())
}

pub async fn process_token_transfer<W: Write>(
    url: &str,
    owner: &str,
    token: &str,
    amount: &str,
    receiver: &str,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let default_decimals = format!("{}", spl_token_2022::native_mint::DECIMALS);
    let minimum_signers_help = spl_token_cli::clap_app::minimum_signers_help_string();
    let multisig_member_help = spl_token_cli::clap_app::multisig_member_help_string();
    let app = spl_token_cli::clap_app::app(
        &default_decimals,
        &minimum_signers_help,
        &multisig_member_help,
    );
    let app_matches = app.get_matches_from(vec![
        "spl-token",
        "transfer",
        "-u",
        url,
        "--fee-payer",
        owner,
        "--owner",
        owner,
        token,
        amount,
        receiver,
    ]);
    let mut wallet_manager = None;
    let mut bulk_signers: Vec<std::sync::Arc<dyn solana_sdk::signer::Signer>> = Vec::new();
    let (sub_command, matches) = app_matches.subcommand().unwrap();
    let sub_command = spl_token_cli::clap_app::CommandName::from_str(sub_command).unwrap();
    let mut multisigner_ids = Vec::new();
    let config = spl_token_cli::config::Config::new(
        matches,
        &mut wallet_manager,
        &mut bulk_signers,
        &mut multisigner_ids,
    )
    .await;
    match spl_token_cli::command::process_command(
        &sub_command,
        matches,
        &config,
        wallet_manager,
        bulk_signers,
    )
    .await
    {
        Ok(s) => {
            writeln!(writer, "{s}")?;
            Ok(())
        }
        Err(e) => Err(format!("{e}").into()),
    }
}

pub fn get_epoch_end_time(rpc_client: &RpcClient) -> Result<String, Box<dyn std::error::Error>> {
    let epoch_info = rpc_client.get_epoch_info()?;
    let last_complete_block_slot = epoch_info.absolute_slot - epoch_info.slot_index - 1;
    let block_time = rpc_client.get_block_time(last_complete_block_slot)?;
    let now_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("invalid now timestamp")
        .as_secs();
    let fraction_done = Decimal::from_u64(epoch_info.slot_index).unwrap()
        / Decimal::from_u64(epoch_info.slots_in_epoch).unwrap();
    let secs_since_epoch =
        Decimal::from_u64(now_timestamp).unwrap() - Decimal::from_i64(block_time).unwrap();
    let secs_to_epoch_end = secs_since_epoch / fraction_done - secs_since_epoch;
    let end_epoch_timestamp = (Decimal::from_u64(now_timestamp).unwrap() + secs_to_epoch_end)
        .to_i64()
        .unwrap();
    let epoch_end_datetime = DateTime::from_timestamp(end_epoch_timestamp, 0)
        .expect("invalid timestamp")
        .with_timezone(&Local);
    Ok(format!("{}", epoch_end_datetime.format("%Y-%m-%d %H:%M")))
}

pub fn get_account_state<W: Write>(
    rpc_clients: &RpcClients,
    token: MaybeToken,
    address: Pubkey,
    writer: &mut W,
) -> Result<(), Box<dyn std::error::Error>> {
    let rpc_client = rpc_clients.default();
    let account = rpc_client.get_account(&address)?;
    if account.owner == system_program::id() {
        if token.is_sol() {
            writeln!(
                writer,
                "Balance: {:.9}",
                solana_sdk::native_token::lamports_to_sol(account.lamports),
            )?;
        } else {
            if let Ok(balance) = token.balance(rpc_client, &address) {
                writeln!(writer, "Balance: {:.9}", token.ui_amount(balance),)?;
            } else {
                writeln!(writer, "Balance: unknown",)?;
            }
        }
    } else if account.owner == solana_sdk::stake::program::id() {
        if let Ok(stake_state) = account.state() {
            let sol = solana_sdk::native_token::lamports_to_sol(account.lamports);
            writeln!(writer, "Balance:            {sol:.9} SOL")?;
            let stake_history_account =
                rpc_client.get_account(&solana_sdk::sysvar::stake_history::id())?;
            let stake_history: solana_sdk::stake_history::StakeHistory =
                solana_sdk::account::from_account(&stake_history_account).unwrap();
            let clock_account = rpc_client.get_account(&solana_sdk::sysvar::clock::id())?;
            let clock: solana_sdk::clock::Clock =
                solana_sdk::account::from_account(&clock_account).unwrap();
            let new_rate_activation_epoch = rpc_client
                .get_feature_activation_slot(
                    &solana_sdk::feature_set::reduce_stake_warmup_cooldown::id(),
                )
                .and_then(|activation_slot: Option<solana_sdk::clock::Slot>| {
                    rpc_client
                        .get_epoch_schedule()
                        .map(|epoch_schedule| (activation_slot, epoch_schedule))
                })
                .map(|(activation_slot, epoch_schedule)| {
                    activation_slot.map(|slot| epoch_schedule.get_epoch(slot))
                })?;

            match stake_state {
                solana_sdk::stake::state::StakeStateV2::Stake(
                    solana_sdk::stake::state::Meta {
                        rent_exempt_reserve: _,
                        authorized,
                        lockup: _,
                    },
                    stake,
                    _,
                ) => {
                    let epoch_end_time = get_epoch_end_time(rpc_client)?;
                    let solana_sdk::stake::state::StakeActivationStatus {
                        effective,
                        activating,
                        deactivating,
                    } = stake.delegation.stake_activating_and_deactivating(
                        clock.epoch,
                        &stake_history,
                        new_rate_activation_epoch,
                    );
                    let sol = solana_sdk::native_token::lamports_to_sol(stake.delegation.stake);
                    writeln!(writer, "Delegated stake:    {sol:.9} SOL")?;
                    if effective > 0 {
                        let sol = solana_sdk::native_token::lamports_to_sol(effective);
                        writeln!(writer, "Active stake:       {sol:.9} SOL")?;
                    }
                    if activating > 0 {
                        let sol = solana_sdk::native_token::lamports_to_sol(activating);
                        writeln!(writer, "Activating stake:   {sol:.9} SOL")?;
                    }
                    if deactivating > 0 {
                        let sol = solana_sdk::native_token::lamports_to_sol(deactivating);
                        writeln!(writer, "Deactivating stake: {sol:.9} SOL")?;
                    }
                    writeln!(writer, "Stake authority:    {}", authorized.staker)?;
                    writeln!(writer, "Withdraw authority: {}", authorized.withdrawer)?;
                    if stake.delegation.voter_pubkey != Pubkey::default() {
                        writeln!(
                            writer,
                            "Vote account:       {}",
                            stake.delegation.voter_pubkey
                        )?;
                    }
                    if deactivating > 0 {
                        writeln!(
                            writer,
                            "Stake will be available for withdrawal after {}",
                            epoch_end_time
                        )?;
                    } else if activating == 0 && effective > 0 {
                        writeln!(
                            writer,
                            "Stake won't be available for withdrawal until {}",
                            epoch_end_time
                        )?;
                    }
                }
                solana_sdk::stake::state::StakeStateV2::RewardsPool => {
                    writeln!(writer, "Rewards pool account")?;
                }
                solana_sdk::stake::state::StakeStateV2::Uninitialized => {
                    writeln!(writer, "Stake account is uninitialized")?;
                }
                solana_sdk::stake::state::StakeStateV2::Initialized(
                    solana_sdk::stake::state::Meta {
                        rent_exempt_reserve: _,
                        authorized,
                        lockup: _,
                    },
                ) => {
                    writeln!(writer, "Stake authority:    {}", authorized.staker)?;
                    writeln!(writer, "Withdraw authority: {}", authorized.withdrawer)?;
                    writeln!(writer, "Stake is available for withdrawal now")?;
                }
            }
        }
    } else {
        writeln!(writer, "improbable account")?;
    }
    Ok(())
}

fn get_account_balance(
    rpc_clients: &RpcClients,
    address: Pubkey,
) -> Result<u64, Box<dyn std::error::Error>> {
    let rpc_client = rpc_clients.default();
    let account = rpc_client.get_account(&address)?;
    Ok(account.lamports)
}

fn adjust_balance(db: &mut Db, address: Pubkey) {
    if let Some(mut account) = db
        .get_accounts()
        .into_iter()
        .find(|x| x.token.is_sol() && x.address == address)
    {
        let rpc = RPC.read().unwrap();
        let network_balance = get_account_balance(&rpc, address).unwrap_or_default();
        if network_balance > 0 && network_balance < account.last_update_balance {
            let fee = account.last_update_balance - network_balance;
            if fee < account.lots[0].amount {
                account.last_update_balance = network_balance;
                account.lots[0].amount -= fee;
                db.update_account(account).unwrap();
            }
        }
    }
}
