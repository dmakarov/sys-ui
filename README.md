# sys-ui

This is a simple GUI to [sys](https://github.com/mvines/sys)

Install [Dioxus](https://dioxuslabs.com/learn/0.6/getting_started/).

### Running

sys-ui expects to find a configuration file at the path:
```bash
~/.config/sys-ui/config.yml
```

The file has to include the following three configuration settings
```yml
db_path: /some/path/to/.sys
json_rpc_url: https://api.mainnet-beta.solana.com
authority_keypair: keypair_path
```

The `db_path` setting is a location of sys database, ie where the file
data.json can be loaded from. The `json_rpc_url` setting is a url to
use for sending solana rpc commands, and normally would be a mainnet
api url, or a hellius url that acts as a proxi to the
mainnet. Finally, `authority_keypair` is a path or a url, such as
`usb://ledger` of a keypair to use for signing transactions. The
latter two settings doesn't have to be set in the file and can be
overridden in the GUI. The location of the sys database needs to be in
the file for GUI to start.

Run the following command in the root of your repository clone:
```bash
dx serve --platform desktop
```

