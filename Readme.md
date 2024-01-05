# stm32-tcp-serial

This STM32 project emulates an USB Ethernet device (CDC-NCM) and runs transparent transmission service between TCP server and serial port. 

Target hardware: [STM32F4DISCOVERY](https://www.st.com/en/evaluation-tools/stm32f4discovery.html)

## Running Code

- Install probe-rs

    ```bash
    cargo install probe-rs --features cli
    ```

- Build and run in release profile 

    Note: It is recommended to default to release profile. The debug profile may be oversize to some STM32 targets. 

    ```
    cargo run --release
    ```

## Tips for build on other chips

- Update chip name defined in `Cargo.toml` and `.cargo/config.toml` to run on other stm32 chips. 

- Remove `--connect-under-reset` option from `.cargo/config.toml` if your debugger doesn't support hardware reset feature. 
