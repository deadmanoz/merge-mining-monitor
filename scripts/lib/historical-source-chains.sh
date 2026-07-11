#!/usr/bin/env bash

historical_source_chain_entries() {
    cat <<'CHAINS'
argentum|arg_height
bitcoin-vault|btcv_height
bitmark|btmk_height
coiledcoin|clc_height
crown|crown_height
devcoin|dvc_height
elcash|elc_height
emercoin|emc_height
geistgeld|geistgeld_height
groupcoin|groupcoin_height
huntercoin|huc_height
i0coin|child_height
ixcoin|ixc_height
myriadcoin|xmy_height
terracoin|trc_height
unobtanium|uno_height
xaya|child_height
CHAINS
}

# Recovered import sources intentionally supplied through `--csv` rather than
# the generated stale-only manifest. Keep these lifecycle rows out of the
# manifest-vs-registry equality check without weakening it for any other source.
explicit_recovery_source_codes() {
    cat <<'SOURCES'
auxpow:lyncoin
auxpow:sixeleven
auxpow:vcash
SOURCES
}
