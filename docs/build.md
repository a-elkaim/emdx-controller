# How to compile

[← Back to the README](../README.md)

## Requirements

- **Linux** and **Rust 1.85** or newer (`rustup` is the easiest way to get it).
- For first-time setup over Bluetooth only: a Bluetooth adapter and **BlueZ**, with its
  experimental API turned on (**required**). The display supports both Bluetooth LE and
  Classic, and without this API BlueZ connects to it over Classic, where the setup service
  doesn't exist; `emdx setup` then stops and tells you to enable it:

  ```bash
  sudo sed -i 's/^#Experimental = false/Experimental = true/' /etc/bluetooth/main.conf
  sudo systemctl restart bluetooth
  ```

## Build

```bash
cargo build --release
./target/release/emdx --help
```

To build without Bluetooth (no first-time setup, everything else works):
`cargo build --release --no-default-features`.

## Run the tests

```bash
cargo test
```

The tests need no display (see [Tests](development.md#tests)).
