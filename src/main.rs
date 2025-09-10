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

#[derive(Clone, Props)]
struct State {
    pub sorted: Option<Sorting>,
    pub selected: BTreeSet<usize>,
    pub amount: Option<f64>,
    pub authority: Option<String>,
    pub recipient: Option<String>,
    pub log: Option<String>,
    pub url: Option<String>,
}

impl PartialEq for State {
    fn eq(&self, _other: &State) -> bool {
        false
    }
}

#[derive(Clone, Copy)]
struct GlobalState {
    db: Signal<std::rc::Rc<Db>>,
    rpc: Signal<std::rc::Rc<RpcClients>>,
    state: Signal<State>,
    prices: Signal<BTreeMap<String, f64>>,
    account: Signal<Option<TrackedAccount>>,
    xaccount: Signal<Option<(Exchange, String)>>,
    xpmethod: Signal<Option<(Exchange, String)>>,
    xclients: Signal<Option<HashMap<Exchange, Box<dyn ExchangeClient>>>>,
    xupdate: Signal<bool>,
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

fn main() {
    dioxus::launch(App);
}

#[component]
fn App() -> Element {
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
    let config: Config = serde_yaml::from_reader(file).unwrap_or_else(|e| {
        eprintln!(
            "Failed to read config from file {}: {:?}",
            conf_clone.display(),
            e
        );
        std::process::exit(1);
    });
    let db_path = std::path::PathBuf::from(config.db_path);
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
    let rpc_clients = RpcClients::new(config.json_rpc_url.clone(), None, None);
    let exchanges = db.get_exchanges();
    let mut xclients = HashMap::<_, _>::new();
    for x in exchanges {
        if let Some(credentials) = db.get_exchange_credentials(x, &"") {
            if let Ok(client) = exchange_client_new(x, credentials) {
                xclients.insert(x, client);
            }
        }
    }
    let _global_state = use_context_provider(|| GlobalState {
        db: Signal::new(std::rc::Rc::new(db)),
        rpc: Signal::new(std::rc::Rc::new(rpc_clients)),
        state: Signal::new(State {
            sorted: None,
            selected: BTreeSet::default(),
            amount: None,
            authority: Some(config.authority_keypair),
            recipient: None,
            log: None,
            url: Some(config.json_rpc_url),
        }),
        prices: Signal::new(BTreeMap::default()),
        account: Signal::new(None),
        xaccount: Signal::new(None),
        xpmethod: Signal::new(None),
        xclients: Signal::new(Some(xclients)),
        xupdate: Signal::new(false),
    });

    use_coroutine(update_prices);

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
    let mut state = use_context::<GlobalState>().state;
    let url = state.read().url.clone().unwrap_or_default();
    rsx! {
        div { id: "menu",
              button {
                  onclick: move |_| {
                      spawn(async move {
                          sync().await
                      });
                  },
                  "Sync"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          split().await
                      });
                  },
                  "Split"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          deactivate().await
                      });
                  },
                  "Deactivate"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          withdraw().await
                      });
                  },
                  "Withdraw"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          delegate().await
                      });
                  },
                  "Delegate"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          swap().await
                      });
                  },
                  "Swap"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          merge().await
                      });
                  },
                  "Merge"
              }
              button {
                  onclick: move |_| {
                      spawn(async move {
                          disburse().await
                      });
                  },
                  "Disburse"
              }
              label {r#for: "json_rpc_url", "url:"}
              input {
                  id: "json_rpc_url",
                  name: "json_rpc_url",
                  value: url,
                  oninput: move |event| {
                      let mut state = state.write();
                      let value = event.value();
                      state.url = Some(value.clone());
                      let mut rpc = use_context::<GlobalState>().rpc;
                      *rpc.write() = std::rc::Rc::new(RpcClients::new(value, None, None));
                  }
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
    let db = use_context::<GlobalState>().db;
    let accounts = db.read().get_accounts();

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

    if let Some(account) = account {
        let content = account.description.to_string();
        let mut buffer = std::io::BufWriter::new(Vec::new());
        let rpc = use_context::<GlobalState>().rpc;
        if get_account_state(&rpc.read(), account.token, account.address, &mut buffer).is_ok() {
            let bytes = buffer.into_inner().unwrap();
            let account_state = String::from_utf8(bytes).unwrap();
            rsx! {
                div { id: "account_state",
                      pre {
                          "{content}",
                          br {},
                          "{account_state}"
                      }
                }
            }
        } else {
            rsx! {
                div { id: "account_state",
                      "{content}"
                }
            }
        }
    } else {
        rsx! {
            div { id: "account_state"}
        }
    }
}

#[component]
pub fn Exchanges() -> Element {
    let db = use_context::<GlobalState>().db;
    let exchanges = db.read().get_exchanges();

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
        return rsx! {div {"Accounts"}};
    }
    let accs = accs.as_ref().unwrap();
    if accs.is_err() {
        return rsx! {div {"Accounts"}};
    }
    rsx! {
            div {
                "Accounts"
            }
            div {
                ul {
                    for acc in accs.as_ref().unwrap() {
                        if acc.value.parse::<f64>().unwrap() > 0. || acc.currency == "USDC" {
                            ExchangeAccountsItem {
                                exchange: exchange.clone(),
                                account: acc.clone(),
                            }
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
                (*xaccount.write(), state.write().recipient) =
                    if is_meta && kind == "selected" {
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
        return rsx! {div {"Payment methods"}};
    }
    let methods = methods.as_ref().unwrap();
    if methods.is_err() {
        return rsx! {div {"Payment methods"}};
    }
    rsx! {
        div {
            "Payment methods"
        }
        div {
            ul {
                for method in methods.as_ref().unwrap() {
                    PaymentMethodsItem {
                        exchange: exchange.clone(),
                        method: method.clone(),
                    }
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
                *selected_method.write() =
                    if is_meta && kind == "selected" {
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

    if let Some(account) = account {
        let prices = use_context::<GlobalState>().prices.read().clone();
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
            div {
                id: "lots",
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
                            "Lot",
                        },
                        th {
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Date(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Date(v));
                            },
                            "Date",
                        },
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
                            "Amount",
                        },
                        th {
                            onclick: move |_| {
                                let sorted = state.read().sorted.clone();
                                let mut v = true;
                                if let Some(Sorting::Price(x)) = sorted {
                                    v = !x;
                                }
                                state.write().sorted = Some(Sorting::Price(v));
                            },
                            "Price",
                        },
                        th {
                            id: "lot_term",
                            "Term",
                        },
                        th {
                            "Gain",
                        },
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
        account.token.format_amount(account.last_update_balance),
    );
    let mut kind = "regular";
    if let Some(selected_account) = selected_account {
        if account.address == selected_account.address && account.token == selected_account.token {
            kind = "selected";
        }
    }
    rsx! {
        li { class: kind,
             onclick: move |event| {
                 let modifiers = event.data().modifiers();
                 let mut state = state.write();
                 if modifiers == Modifiers::ALT {
                     state.recipient = Some(account.address.to_string());
                     return;
                 }
                 let is_meta = modifiers.meta() || (modifiers.alt() && modifiers.ctrl());
                 if is_meta || kind == "regular" {
                     state.selected.clear();
                 }
                 let mut selected_account = use_context::<GlobalState>().account;
                 *selected_account.write() =
                     if is_meta && kind == "selected" {
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
    let state = use_context::<GlobalState>().state;
    let selected = &state.read().selected;
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
    let kind = if selected.contains(&lot.lot_number) {
        "selected"
    } else {
        "regular"
    };
    rsx! {
        tr {
            class: kind,
            onclick: move |event| { selection(lot.lot_number, &event); },
            td { class: "lot_number", "{lot_number}" },
            td { class: "lot_date", "{lot_date}" },
            td { class: "lot_amount", "{lot_amount}" },
            td { "{lot_price}" },
            td { class: "lot_term", "{term}" },
            td { "{gain}" },
        }
    }
}

#[component]
fn Tokens() -> Element {
    let prices = use_context::<GlobalState>().prices.read().clone();
    rsx! {
        div { id: "tokens",
              table {
                  for (token, price) in prices.into_iter() {
                      tr { class: "token",
                           onclick: move |_| {
                               let mut state = use_context::<GlobalState>().state;
                               state.write().recipient = Some(token.to_string());
                           },
                          td { class: "token", "{token}" },
                          td { class: "token", "${price}" },
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
              label {r#for: "authority", "authority:"}
              input {
                  id: "authority",
                  name: "authority",
                  value: authority,
                  oninput: move |event| state.write().authority = Some(event.value())
              }
              label {r#for: "recipient", "recipient:"}
              input {
                  id: "recipient",
                  name: "recipient",
                  value: recipient,
                  oninput: move |event| {
                      let value = event.value();
                      state.write().recipient = if value.is_empty() {
                          None
                      } else {
                          Some(value)
                      }
                  }
              }
              label {r#for: "amount", "amount:"}
              input {
                  id: "amount",
                  name: "amount",
                  value: amount,
                  oninput: move |event| {
                      let value = event.value();
                      if !value.ends_with(".") && !(value.contains(".") && value.ends_with("0")) {
                          state.write().amount = value.parse::<f64>().ok();
                      }
                  }
              }
        }
    }
}

#[component]
pub fn Summary() -> Element {
    let selected_account = use_context::<GlobalState>().account.read().clone();
    let prices = use_context::<GlobalState>().prices.read().clone();
    let state = use_context::<GlobalState>().state;
    let db = use_context::<GlobalState>().db;
    let (long_term_gain_tax_rate, short_term_gain_tax_rate) =
        if let Some(ref rate) = db.read().get_tax_rate() {
            (rate.long_term_gain, rate.short_term_gain)
        } else {
            (0.22f64, 0.3935f64)
        };
    let accounts = db.read().get_accounts();
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
    if !state.read().selected.is_empty() {
        let account = use_context::<GlobalState>().account.read().clone();
        if let Some(account) = account {
            let selected_lots_value = account
                .lots
                .iter()
                .filter(|x| state.read().selected.contains(&x.lot_number))
                .fold(0u64, |acc, x| acc + x.amount);
            let cost = account
                .lots
                .iter()
                .filter(|x| state.read().selected.contains(&x.lot_number))
                .fold(0f64, |acc, x| {
                    acc + x.acquisition.price().to_f64().unwrap()
                        * account.token.ui_amount(x.amount)
                });
            let today = chrono::Local::now().date_naive();
            let (short_gain, long_gain) = account
                .lots
                .iter()
                .filter(|x| state.read().selected.contains(&x.lot_number))
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
        div { id: "summary",
              "{summary}"
        }
    }
}

#[component]
pub fn Log() -> Element {
    let state = use_context::<GlobalState>().state;
    let log = state.read().log.clone();
    if let Some(content) = log {
        rsx! {
            div { id: "log",
                  pre {
                      "{content}"
                  }
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
    let db = use_context::<GlobalState>().db;
    let disposed_lots = db.read().disposed_lots();

    rsx! {
        div { id: "disposed",
            table {
                thead {
                    tr {
                        th { "Lot" },
                        th { "Sale Date" },
                        th { "Acq Date" },
                        th { "Amount" },
                        th { "Sale Price" },
                        th { "Acq Price" },
                        th { "Cap Gain" },
                        th { "Term" },
                    }
                }
                tbody {
                    for lot in disposed_lots {
                        DisposedLotItem {
                            lot: lot.clone()
                        }
                    }
                }
            }
        }
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
    let gain = format!(
        "{}",
        (lot.token.ui_amount(lot.lot.amount) * (sale_price - acq_price))
            .separated_string_with_fixed_place(2)
    );
    let acq_price = format!("${}", acq_price.separated_string_with_fixed_place(2));
    let sale_price = format!("${}", sale_price.separated_string_with_fixed_place(2));
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

    rsx! {
        tr {
            td { class: "lot_number", "{lot_number}" },
            td { class: "lot_date", "{sale_date}" },
            td { class: "lot_date", "{acq_date}" },
            td { class: "lot_amount", "{amount}" },
            td { "{sale_price}" },
            td { "{acq_price}" },
            td { "{gain}" },
            td { class: "lot_term", "{term}" },
        }
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

fn selection(lot: usize, event: &Event<MouseData>) {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    let account = use_context::<GlobalState>().account.read().clone();
    let modifiers = event.data().modifiers();
    if modifiers.shift() {
        if let Some(account) = account {
            let mut sel_end = 0;
            let mut sel_beg = 0;
            for (i, l) in account.lots.iter().enumerate() {
                if l.lot_number == lot {
                    sel_end = i;
                } else if state.selected.contains(&l.lot_number) {
                    sel_beg = i;
                }
            }
            if sel_beg > sel_end {
                std::mem::swap(&mut sel_beg, &mut sel_end);
            }
            for i in sel_beg..=sel_end {
                state.selected.insert(account.lots[i].lot_number);
            }
        }
    } else if modifiers.meta() {
        if state.selected.contains(&lot) {
            state.selected.remove(&lot);
        } else {
            state.selected.insert(lot);
        }
    } else {
        state.selected.clear();
        state.selected.insert(lot);
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
    {$signer:ident, $state:ident} => {
        {
            let arg_matches = make_arg_matches!("by", $signer, is_valid_signer);
            let mut wallet_manager = None;
            let (signer, address) = match signer_of(&arg_matches, "by", &mut wallet_manager) {
                Ok(v) => v,
                Err(e) => {
                    $state.log = Some(format!("Invalid signer {}: {:?}", $signer, e));
                    return;
                }
            };
            (signer.expect("signer"), address.expect("address"))
        }
    }
}

async fn sync() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let mut db = use_context::<GlobalState>().db;
    let rpc = use_context::<GlobalState>().rpc;
    let account = use_context::<GlobalState>().account.read().clone();
    let address = account.map(|x| x.address);
    let reconcile_no_sync_account_balances = false;
    let force_rescan_balances = false;
    let max_epochs_to_process = None;
    let notifier = Notifier::default();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_account_sync(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        &rpc.read(),
        address,
        max_epochs_to_process,
        reconcile_no_sync_account_balances,
        force_rescan_balances,
        &notifier,
        &mut buffer,
    )
    .await
    {
        state.log = Some(format!("Failed sys account sync {:?}: {:?}", address, e));
        return;
    }
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
    let mut xupdate = use_context::<GlobalState>().xupdate;
    *xupdate.write() = true; // value doesn't matter, just need to rewrite it
}

async fn split() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let mut selected_account = use_context::<GlobalState>().account;
    if state.selected.is_empty() || selected_account.read().is_none() {
        state.log = Some("Select account and lots to split".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log = Some("Enter staking authority keypair for account to be split".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let mut db = use_context::<GlobalState>().db;
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let amount = account
        .lots
        .iter()
        .filter(|x| state.selected.contains(&x.lot_number))
        .fold(0, |acc, x| acc + x.amount);
    let description = None;
    let lot_selection_method = LotSelectionMethod::default();
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| state.selected.contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, state);
    let recipient = state.recipient.clone();
    let to_keypair = recipient.map(|r| {
        let arg_matches = make_arg_matches!("to", r, is_keypair);
        keypair_of(&arg_matches, "to").unwrap()
    });
    let if_balance_exceeds = None;
    let priority_fee = PriorityFee::default_auto();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_account_split(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        &rpc.read(),
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
        state.log = Some(format!(
            "Failed sys account split {:?} {}: {:?}",
            account.address,
            account.token.format_amount(amount),
            e,
        ));
        return;
    }
    *selected_account.write() = None;
    state.selected.clear();
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn deactivate() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let selected_account = use_context::<GlobalState>().account;
    if selected_account.read().is_none() {
        state.log = Some("Select account to deactivate".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log =
            Some("Enter staking authority keypair for account to be deactivated".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let account = selected_account.read().clone().unwrap();
    let authority = state.authority.clone().unwrap();
    state.log = Some(format!(
        "deactivate-stake --stake-authority {} {}\nCheck ledger device for signing",
        authority, account.address,
    ));
    let (authority_signer, authority_address) = make_signer!(authority, state);
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_stake_deactivate(
        &rpc.read(),
        account.address,
        authority_address,
        vec![authority_signer],
        &mut buffer,
    )
    .await
    {
        state.log = Some(format!(
            "Failed solana deactivate-stake --stake-authority {:?} {:?}: {:?}",
            authority, account.address, e,
        ));
    }
    let mut selected_account = use_context::<GlobalState>().account;
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn withdraw() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let mut selected_account = use_context::<GlobalState>().account;
    if state.selected.is_empty() || selected_account.read().is_none() {
        state.log = Some("Select account and lots to withdraw".to_string());
        return;
    }
    if state.recipient.is_none() {
        state.log = Some("Enter account address to deposit the withdrawn funds to".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log =
            Some("Enter withdraw authority keypair for account to withdraw from".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let mut db = use_context::<GlobalState>().db;
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let amount = if state.amount.unwrap_or_default() > 0. {
        account.token.amount(state.amount.unwrap())
    } else {
        account
            .lots
            .iter()
            .filter(|x| state.selected.contains(&x.lot_number))
            .fold(0, |acc, x| acc + x.amount)
    };
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| state.selected.contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let lot_selection_method = LotSelectionMethod::default();
    let recipient = state.recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let to_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            state.log = Some(format!("Invalid address to deposit to {}", recipient));
            return;
        }
    };
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if !account.token.is_sol() {
        if let Err(e) = process_token_transfer(
            &state.url.clone().unwrap(),
            &state.authority.clone().unwrap(),
            &account.token.mint().to_string(),
            &format!("{}", account.token.ui_amount(amount)),
            &state.recipient.clone().unwrap(),
            &mut buffer,
        )
        .await
        {
            state.log = Some(format!(
                "Failed spl-token transfer --owner {:?} {}: {:?}",
                account.address,
                account.token.format_amount(amount),
                e,
            ));
            return;
        }
        *selected_account.write() = None;
        state.selected.clear();
        if let Err(e) = std::rc::Rc::get_mut(&mut db.write()).unwrap().record_drop(
            account.address,
            account.token,
            amount,
            LotSelectionMethod::LastInFirstOut,
            Some(lot_numbers),
        ) {
            state.log = Some(format!("Failed to drop lots: {e:#?}"));
        }
        let bytes = buffer.into_inner().unwrap();
        state.log = Some(String::from_utf8(bytes).unwrap());
        // Force fetching account data from exchange
        let mut xupdate = use_context::<GlobalState>().xupdate;
        *xupdate.write() = true; // value doesn't matter, just need to rewrite it
        return;
    }
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, state);
    let custodian = None;
    if let Err(e) = process_stake_withdraw(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        &rpc.read(),
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
        state.log = Some(format!(
            "Failed solana withdraw-stake {:?} {}: {:?}",
            account.address,
            account.token.format_amount(amount),
            e,
        ));
        return;
    }
    *selected_account.write() = None;
    state.selected.clear();
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn delegate() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let selected_account = use_context::<GlobalState>().account;
    if selected_account.read().is_none() {
        state.log = Some("Select account to delegate".to_string());
        return;
    }
    if state.recipient.is_none() {
        state.log = Some("Enter validator address to delegate to".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log = Some("Enter staking authority keypair for account to delegate".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let recipient = state.recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let to_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            state.log = Some(format!("Invalid validator address {}", recipient));
            return;
        }
    };
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, state);
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_stake_delegate(
        &rpc.read(),
        from_address,
        authority_address,
        to_address,
        vec![authority_signer],
        &mut buffer,
    )
    .await
    {
        state.log = Some(format!(
            "Failed solana delegate-stake --stake-authority {} {} {}: {:?}",
            authority, from_address, to_address, e,
        ));
    }
    let mut selected_account = use_context::<GlobalState>().account;
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn swap() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let mut selected_account = use_context::<GlobalState>().account;
    if state.selected.is_empty() || selected_account.read().is_none() {
        state.log = Some("Select account and lots to swap".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log = Some("Enter signer keypair for swap".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let mut db = use_context::<GlobalState>().db;
    let account = selected_account.read().clone().unwrap();
    let authority = state.authority.clone().unwrap();
    let (signer, address) = make_signer!(authority, state);
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
        .filter(|x| state.selected.contains(&x.lot_number))
        .fold(0, |acc, x| acc + x.amount);
    let ui_amount = Some(from_token.ui_amount(amount));
    let slippage_bps = 100u64;
    let lot_selection_method = LotSelectionMethod::LastInFirstOut;
    let lot_numbers = account
        .lots
        .iter()
        .filter(|x| state.selected.contains(&x.lot_number))
        .map(|x| x.lot_number)
        .collect();
    let signature = None;
    let if_from_balance_exceeds = None;
    let for_no_less_than = None;
    let max_coingecko_value_percentage_loss = 5f64;
    let priority_fee = PriorityFee::default_auto();
    let notifier = Notifier::default();
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_jup_swap(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        &rpc.read(),
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
        state.log = Some(format!(
            "Failed sys jup swap {:?} {} {} {}: {:?}",
            authority,
            from_token,
            to_token,
            from_token.ui_amount(amount),
            e,
        ));
        return;
    }
    if let Err(e) = process_sync_swaps(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        rpc.read().default(),
        &notifier,
        &mut buffer,
    )
    .await
    {
        state.log = Some(format!("Failed sys sync: {:?}", e,));
        return;
    }
    *selected_account.write() = None;
    state.selected.clear();
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn merge() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    state.log = None;
    let mut selected_account = use_context::<GlobalState>().account;
    if selected_account.read().is_none() {
        state.log = Some("Select account to merge".to_string());
        return;
    }
    if state.recipient.is_none() {
        state.log = Some("Enter account address to be merged into".to_string());
        return;
    }
    if state.authority.is_none() {
        state.log = Some("Enter staking authority keypair for account to be merged".to_string());
        return;
    }
    let rpc = use_context::<GlobalState>().rpc;
    let mut db = use_context::<GlobalState>().db;
    let account = selected_account.read().clone().unwrap();
    let from_address = account.address;
    let recipient = state.recipient.clone().unwrap();
    let arg_matches = make_arg_matches!("to", recipient, is_valid_pubkey);
    let into_address = match pubkey_of(&arg_matches, "to") {
        Some(v) => v,
        None => {
            state.log = Some(format!("Invalid address to merge into {}", recipient));
            return;
        }
    };
    let authority = state.authority.clone().unwrap();
    let (authority_signer, authority_address) = make_signer!(authority, state);
    let priority_fee = PriorityFee::default_auto();
    let signature = None;
    let mut buffer = std::io::BufWriter::new(Vec::new());
    if let Err(e) = process_account_merge(
        std::rc::Rc::get_mut(&mut db.write()).unwrap(),
        &rpc.read(),
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
        state.log = Some(format!(
            "Failed sys account merge {:?} --into {:?}: {:?}",
            from_address, into_address, e,
        ));
        return;
    }
    *selected_account.write() = None;
    let bytes = buffer.into_inner().unwrap();
    state.log = Some(String::from_utf8(bytes).unwrap());
}

async fn disburse() {
    let mut state = use_context::<GlobalState>().state;
    let mut state = state.write();
    let xclients = use_context::<GlobalState>().xclients;
    let xclients = xclients.read();
    let xaccount = use_context::<GlobalState>().xaccount.read().clone();
    if xaccount.is_none() {
        state.log = Some("Select exchange account from which to disburse cash".to_string());
        return;
    }
    let xpmethod = use_context::<GlobalState>().xpmethod.read().clone();
    if xpmethod.is_none() {
        state.log = Some("Select bank account to which to disburse cash".to_string());
        return;
    }
    let (exchange, account) = xaccount.unwrap();
    let client = xclients.as_ref().unwrap().get(&exchange).unwrap();
    let amount = if state.amount.clone().unwrap_or_default() > 0. {
        state.amount.clone().unwrap_or_default().to_string()
    } else {
        let accounts = client.accounts().await;
        if let Err(e) = accounts {
            state.log = Some(format!("Couldn't get exchange accounts {e}"));
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
        state.log = Some(format!("Couldn't get exchange payment methods {e}"));
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
            state.log = Some(format!(
                "Disbursed cash ${}, fee ${}, reference {} {:#?}",
                d.total, d.total_fee, d.user_reference, d.user_warnings,
            ))
        }
        Err(e) => state.log = Some(format!("{e}")),
    }
    let mut xupdate = use_context::<GlobalState>().xupdate;
    *xupdate.write() = true; // value doesn't matter, just need to rewrite it
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

enum Action {}

async fn update_prices(mut _rx: UnboundedReceiver<Action>) {
    loop {
        let rpc = use_context::<GlobalState>().rpc;
        let mut prices = use_context::<GlobalState>().prices;
        let mut tokens = vec![MaybeToken::from(None)];
        tokens.append(
            &mut Token::VARIANTS
                .into_iter()
                .map(|x| MaybeToken::from(Some(x)))
                .collect::<Vec<MaybeToken>>(),
        );
        for token in tokens {
            let price = token
                .get_current_price(&rpc.read().default())
                .await
                .map(|x| format!("{x:.6}").trim().parse::<f64>().unwrap())
                .unwrap_or(0f64);
            prices.write().insert(token.to_string(), price);
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}
