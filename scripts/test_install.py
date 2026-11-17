#!/usr/bin/env python3
"""Linux-only offline installer integration test; never touches the real HOME."""
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import subprocess
import tempfile
import threading

import package


def main():
    if not os.sys.platform.startswith("linux"):
        raise SystemExit("Run this integration test on Linux")
    with tempfile.TemporaryDirectory(prefix="drcom-install-test-") as temp:
        root = Path(temp)
        assets = root / "assets"
        assets.mkdir()
        stem = "drcom-campus-linux-x86_64-v9.8.7"
        stage = root / stem
        stage.mkdir()
        for binary in package.BINS:
            package.write_text(stage / binary, "#!/bin/sh\nexit 0\n")
            (stage / binary).chmod(0o755)
        package.copy(package.ROOT / "packaging/install.sh", stage / "install.sh", executable=True)
        package.copy(package.ROOT / "packaging/drcom-campus.desktop", stage / "drcom-campus.desktop")
        package.copy(package.ROOT / "crates/drcom-gui/assets/icon-128.png",
                     stage / "share/icons/hicolor/128x128/apps/drcom-campus.png")
        for name in package.LEGAL:
            package.copy(package.ROOT / name, stage / name)
        archive = assets / (stem + ".tar.gz")
        package.tar_tree(stage, archive)
        package.write_checksums([archive], assets)
        handler = partial(SimpleHTTPRequestHandler, directory=str(assets))
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        home = root / "home with spaces"
        home.mkdir()
        env = {**os.environ, "HOME": str(home), "XDG_DATA_HOME": str(home / "data"),
               "XDG_CONFIG_HOME": str(home / "config"), "XDG_BIN_HOME": str(home / "bin"),
               "DRCOM_INSTALL_BASE_URL": f"http://127.0.0.1:{server.server_port}"}
        installer = ["bash", str(package.ROOT / "install.sh"), "--version", "9.8.7", "--no-modify-path"]
        try:
            subprocess.run(installer + ["--autostart"], env=env, check=True)
            for binary in package.BINS:
                assert (home / "bin" / binary).is_file(), binary
            entry = home / "data/applications/drcom-campus.desktop"
            subprocess.run(["desktop-file-validate", str(entry)], check=True)
            assert (home / "config/autostart/drcom-campus.desktop").is_file()
            # Installing twice is safe. --no-autostart must not delete an existing entry.
            subprocess.run(installer + ["--no-autostart"], env=env, check=True)
            assert (home / "config/autostart/drcom-campus.desktop").is_file()
            autostart = home / "config/autostart/drcom-campus.desktop"
            autostart.write_text(autostart.read_text() + "Hidden=true\nX-GNOME-Autostart-enabled=false\n")
            disabled = autostart.read_bytes()
            subprocess.run(installer, env=env, check=True)
            assert autostart.read_bytes() == disabled, "Normal upgrade re-enabled disabled autostart"
            credentials = home / "data/drcom-campus/preferences.dat"
            package.write_text(credentials, "retained test credential placeholder")
            subprocess.run(["bash", str(package.ROOT / "install.sh"), "--uninstall"], env=env, check=True)
            assert credentials.is_file()
            assert not any((home / "bin" / binary).exists() for binary in package.BINS)
            # Tampering must stop before install; missing checksum is also a hard failure.
            archive.write_bytes(b"tampered archive")
            assert subprocess.run(installer, env=env).returncode != 0
            assert not (home / "bin/drcom-campus").exists()
            Path(str(archive) + ".sha256").unlink()
            assert subprocess.run(installer, env=env).returncode != 0
            print("Installer integration passed: install/upgrade/uninstall/retention/checksum hard failures")
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    main()
