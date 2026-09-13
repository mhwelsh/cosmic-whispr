name := 'cosmic-whispr'
appid := 'com.kannerwelsh.CosmicWhispr'

rootdir := ''
prefix := env('HOME') / '.local'
base-dir := absolute_path(clean(rootdir / prefix))

bin-src := 'target' / 'release' / name
bin-dst := base-dir / 'bin' / name
desktop-src := 'data' / appid + '.desktop'
desktop-dst := base-dir / 'share' / 'applications' / appid + '.desktop'
metainfo-src := 'data' / appid + '.metainfo.xml'
metainfo-dst := base-dir / 'share' / 'metainfo' / appid + '.metainfo.xml'
# Flathub expects each module's license under share/licenses/$FLATPAK_ID.
license-dst := base-dir / 'share' / 'licenses' / appid / 'LICENSE'

default: build-release

build-debug *args:
    cargo build {{args}}

build-release *args:
    cargo build --release {{args}}

check *args:
    cargo clippy --all-targets {{args}} -- -D warnings

# Validate the packaging metadata the stores read.
validate:
    appstreamcli validate {{metainfo-src}}
    desktop-file-validate {{desktop-src}}

test *args:
    cargo test {{args}}

# Install for the current user. Pass prefix=/usr rootdir=... for a system install.
install: build-release
    install -Dm0755 {{bin-src}} {{bin-dst}}
    install -Dm0644 {{desktop-src}} {{desktop-dst}}
    install -Dm0644 {{metainfo-src}} {{metainfo-dst}}
    install -Dm0644 LICENSE {{license-dst}}
    # Absolute Exec: the panel's PATH may not include a user-prefix bin dir.
    sed -i 's|^Exec=cosmic-whispr$|Exec={{bin-dst}}|' {{desktop-dst}}
    @echo
    @echo 'Installed. Add "Whispr Dictation" in Settings > Desktop > Panel > Applets,'
    @echo 'then bind a shortcut to: {{bin-dst}} --toggle'

uninstall:
    rm -f {{bin-dst}} {{desktop-dst}} {{metainfo-dst}} {{license-dst}}
    rmdir {{base-dir}}/share/licenses/{{appid}} 2>/dev/null || true

# Restart the panel so a freshly installed applet is picked up.
reload-panel:
    pkill -HUP cosmic-panel || true
