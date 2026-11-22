# Upstream sources used as the conformance reference. They're copied into conformance/.work,
# never modified in place.
moonraker_src := env("MOONRAKER_SRC", home_directory() / "sources/moonraker")
klipper_src := env("KLIPPER_SRC", home_directory() / "sources/klipper")
work := "conformance/.work"
# SSH connections to printers, shared by scripts/copy-to-printer.sh. %C is a hash of the user,
# host, and port.
ssh_control := "/tmp/orrery-ssh-" + `id -u`

# Build, unit tests, then the conformance suite against the reference.
default: test conformance

test:
    cargo test --workspace

lint:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings

# Run the conformance cases. Usage: just conformance [reference|orrery] [extra runner flags]
conformance target="reference" *flags:
    cargo build -p conformance {{ if target == "orrery" { "-p orrery -p orrery-update" } else { "" } }}
    cargo run -q -p conformance -- --target {{target}} {{flags}}

# Run the real-klippy cases against real Klipper in batch mode.
conformance-real target="reference" *flags: (conformance target "--real-klipper" flags)

# Record response-shape snapshots from the reference.
record *flags: (conformance "reference" "--record" flags)

# Install upstream Moonraker into conformance/.work/ref-venv.
ref-setup:
    mkdir -p {{work}}
    rsync -a --delete --exclude .git --exclude .jj --exclude .venv {{moonraker_src}}/ {{work}}/moonraker-src/
    uv venv -q --allow-existing --python 3.12 {{work}}/ref-venv
    VIRTUAL_ENV={{work}}/ref-venv uv pip install -q --reinstall-package moonraker {{work}}/moonraker-src

# Install Klipper into conformance/.work for the real-Klipper tier.
klipper-setup:
    mkdir -p {{work}}
    rsync -a --delete --exclude .git --exclude .jj --exclude out {{klipper_src}}/ {{work}}/klipper-src/
    # Without git metadata Klipper reports its version as "?"; keep the real one.
    git -C {{klipper_src}} describe --tags --always > {{work}}/klipper-src/klippy/.version
    uv venv -q --allow-existing --python 3.12 {{work}}/klipper-venv
    VIRTUAL_ENV={{work}}/klipper-venv uv pip install -q -r {{work}}/klipper-src/scripts/klippy-requirements.txt

# Rebuild the Linux host-MCU data dictionary in conformance/fixtures/klipper (needs gcc).
klipper-dict: klipper-setup
    printf 'CONFIG_MACH_LINUX=y\n' > {{work}}/klipper-src/.config
    make -C {{work}}/klipper-src olddefconfig > /dev/null
    make -C {{work}}/klipper-src -j8 > /dev/null
    cp {{work}}/klipper-src/out/klipper.dict conformance/fixtures/klipper/klipper.dict

# Regenerate conformance/http_routes.json from the reference's endpoint table.
routes:
    rm -rf {{work}}/routes-data && mkdir -p {{work}}/routes-data/config
    sed -e 's/${port}/7125/' -e 's|${klippy_socket}|/tmp/klippy.sock|' conformance/fixtures/moonraker/base.conf > {{work}}/routes-data/config/moonraker.conf
    {{work}}/ref-venv/bin/python -I conformance/tools/dump_routes.py $PWD/{{work}}/routes-data conformance/http_routes.json > {{work}}/routes-data/dump.log 2>&1

# Re-record base_printer.capture.json from real Klipper running the fixture printer.cfg.
capture:
    #!/usr/bin/env bash
    set -euo pipefail
    dir=$PWD/{{work}}/capture
    rm -rf "$dir" && mkdir -p "$dir/gcodes"
    sed "s|\${gcode_path}|$dir/gcodes|" conformance/fixtures/klipper/printer.cfg > "$dir/printer.cfg"
    # The ceiling keeps git from finding this repository, so Klipper reports klippy/.version.
    GIT_CEILING_DIRECTORIES=$PWD/{{work}} {{work}}/klipper-venv/bin/python {{work}}/klipper-src/klippy/klippy.py "$dir/printer.cfg" \
        -o /dev/null -d conformance/fixtures/klipper/klipper.dict -a "$dir/uds" -I "$dir/serial" \
        -l "$dir/klippy.log" > "$dir/stdout.log" 2>&1 &
    pid=$!
    trap 'kill $pid' EXIT
    for _ in $(seq 100); do [ -S "$dir/uds" ] && break; sleep 0.1; done
    sleep 1
    cargo run -q -p fake-klippy -- capture --socket "$dir/uds" --out conformance/klippy-scripts/base_printer.capture.json

# Cross-compile release binaries for the Pi targets (no zig or C toolchain needed).
build-arm:
    cargo build --release -p orrery --target aarch64-unknown-linux-musl
    cargo build --release -p orrery --target armv7-unknown-linux-musleabihf

# Build orrery and its tools for a printer's CPU and copy them to ~/orrery-test/bin on the printer.
copy-to-printer host:
    scripts/copy-to-printer.sh {{host}}

# Keep one SSH connection to a printer open, shared by ssh, scp, and copy-to-printer; closes after `idle` unused.
connect-printer host idle="12h":
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p {{ssh_control}}
    chmod 700 {{ssh_control}}
    if ssh -o ControlPath='{{ssh_control}}/%C' -O check '{{host}}' 2>/dev/null; then
        echo "The SSH connection to {{host}} is already open."
        exit 0
    fi
    ssh -fN -o ControlMaster=yes -o ControlPath='{{ssh_control}}/%C' -o ControlPersist='{{idle}}' '{{host}}'
    echo "Opened an SSH connection to {{host}}. It closes after {{idle}} unused."
    echo "To use it from another command, add: -o ControlPath={{ssh_control}}/%C"

# Close the SSH connection that connect-printer or copy-to-printer opened.
disconnect-printer host:
    #!/usr/bin/env bash
    set -euo pipefail
    if ssh -o ControlPath='{{ssh_control}}/%C' -O exit '{{host}}' 2>/dev/null; then
        echo "Closed the SSH connection to {{host}}."
    else
        echo "There's no open SSH connection to {{host}}."
    fi

# Run orrery on a printer as a temporary systemd unit, orrery-test: start, stop, status, or log.
printer-test command host *args:
    scripts/printer-test.sh {{command}} {{host}} {{args}}
