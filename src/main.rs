#![no_std]
#![no_main]

use defmt::*;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::{Stack, StackResources};
use embassy_stm32::peripherals::{DMA1_CH1, DMA1_CH3, USART3};
use embassy_stm32::rng::{self, Rng};
use embassy_stm32::time::Hertz;
use embassy_stm32::usart;
use embassy_stm32::usart::{Uart, UartRx, UartTx};
use embassy_stm32::{bind_interrupts, peripherals, usb_otg};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::Timer;
use embassy_usb::class::cdc_ncm::embassy_net::{Device, Runner, State as NetState};
use embassy_usb::class::cdc_ncm::{CdcNcmClass, State};
use embassy_usb::{Builder, UsbDevice};
use embedded_io_async::Write;
use heapless::Vec;
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

type UsbDriver = usb_otg::Driver<'static, embassy_stm32::peripherals::USB_OTG_FS>;

const MTU: usize = 1514;
const BUFFER_SIZE_TCP_TO_USART: usize = 500;
const BUFFER_SIZE_USART_TO_TCP: usize = 500;

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn usb_ncm_task(class: Runner<'static, UsbDriver, MTU>) -> ! {
    class.run().await
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<Device<'static, MTU>>) -> ! {
    stack.run().await
}

static CHANNEL_USART_TO_TCP: Channel<ThreadModeRawMutex, Vec<u8, BUFFER_SIZE_USART_TO_TCP>, 1> = Channel::new();
static CHANNEL_TCP_TO_USART: Channel<ThreadModeRawMutex, Vec<u8, BUFFER_SIZE_TCP_TO_USART>, 1> = Channel::new();

#[embassy_executor::task]
async fn usart_reader(mut rx: UartRx<'static, USART3, DMA1_CH1>) {
    loop {
        let mut vec: Vec<u8, BUFFER_SIZE_USART_TO_TCP> = Vec::new();
        vec.resize(BUFFER_SIZE_USART_TO_TCP, 0).unwrap();
        let len = rx.read_until_idle(&mut vec).await.unwrap();
        vec.resize(len, 0).unwrap();
        CHANNEL_USART_TO_TCP.send(vec).await;
    }
}

#[embassy_executor::task]
async fn usart_sender(mut tx: UartTx<'static, USART3, DMA1_CH3>) {
    loop {
        let vec = CHANNEL_TCP_TO_USART.receive().await;
        tx.write(&vec).await.unwrap();
    }
}

bind_interrupts!(struct Irqs {
    OTG_FS => usb_otg::InterruptHandler<peripherals::USB_OTG_FS>;
    RNG => rng::InterruptHandler<peripherals::RNG>;
    USART3 => usart::InterruptHandler<peripherals::USART3>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    info!("Config RCC");
    let mut config = embassy_stm32::Config::default();
    {
        use embassy_stm32::rcc::*;
        config.rcc.hse = Some(Hse {
            freq: Hertz(8_000_000),
            mode: HseMode::Bypass,
        });
        config.rcc.pll_src = PllSource::HSE;
        config.rcc.pll = Some(Pll {
            prediv: PllPreDiv::DIV4,
            mul: PllMul::MUL168,
            divp: Some(PllPDiv::DIV2), // 8mhz / 4 * 168 / 2 = 168Mhz.
            divq: Some(PllQDiv::DIV7), // 8mhz / 4 * 168 / 7 = 48Mhz.
            divr: None,
        });
        config.rcc.ahb_pre = AHBPrescaler::DIV1;
        config.rcc.apb1_pre = APBPrescaler::DIV4;
        config.rcc.apb2_pre = APBPrescaler::DIV2;
        config.rcc.sys = Sysclk::PLL1_P;
    }
    let p = embassy_stm32::init(config);

    info!("Config USART");
    let config = usart::Config::default();
    let usart = Uart::new(p.USART3, p.PD9, p.PD8, Irqs, p.DMA1_CH3, p.DMA1_CH1, config).unwrap();

    let (tx, rx) = usart.split();
    unwrap!(spawner.spawn(usart_sender(tx)));
    unwrap!(spawner.spawn(usart_reader(rx)));

    info!("Config and spawn USB task");
    static OUTPUT_BUFFER: StaticCell<[u8; 256]> = StaticCell::new();
    let ep_out_buffer = &mut OUTPUT_BUFFER.init([0; 256])[..];
    let mut config = embassy_stm32::usb_otg::Config::default();
    config.vbus_detection = true;
    let driver = usb_otg::Driver::new_fs(p.USB_OTG_FS, Irqs, p.PA12, p.PA11, ep_out_buffer, config);

    // Create embassy-usb Config
    let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
    config.manufacturer = Some("Embassy");
    config.product = Some("USB-Ethernet example");
    config.serial_number = Some("12345678");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    // Required for Windows support.
    config.composite_with_iads = true;
    config.device_class = 0xEF;
    config.device_sub_class = 0x02;
    config.device_protocol = 0x01;

    // Create embassy-usb DeviceBuilder using the driver and config.
    static DEVICE_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 128]> = StaticCell::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut DEVICE_DESC.init([0; 256])[..],
        &mut CONFIG_DESC.init([0; 256])[..],
        &mut BOS_DESC.init([0; 256])[..],
        &mut [], // no msos descriptors
        &mut CONTROL_BUF.init([0; 128])[..],
    );

    // Our MAC addr.
    let our_mac_addr = [0xCC, 0xCC, 0xCC, 0xCC, 0xCC, 0xCC];
    // Host's MAC addr. This is the MAC the host "thinks" its USB-to-ethernet adapter has.
    let host_mac_addr = [0x88, 0x88, 0x88, 0x88, 0x88, 0x88];

    // Create classes on the builder.
    static STATE: StaticCell<State> = StaticCell::new();
    let class = CdcNcmClass::new(&mut builder, STATE.init(State::new()), host_mac_addr, 64);

    // Build the builder.
    let usb = builder.build();

    unwrap!(spawner.spawn(usb_task(usb)));
    // FIX ME: Linux CDC-NCM driver requests usb task to be started before cdc_ncm task 
    Timer::after_millis(1000).await;

    info!("Config and spawn ncm task");
    static NET_STATE: StaticCell<NetState<MTU, 4, 4>> = StaticCell::new();
    let (runner, device) = class.into_embassy_net_device::<MTU, 4, 4>(NET_STATE.init(NetState::new()), our_mac_addr);
    unwrap!(spawner.spawn(usb_ncm_task(runner)));

    // let config = embassy_net::Config::dhcpv4(Default::default());
    let config = embassy_net::Config::ipv4_static(embassy_net::StaticConfigV4 {
        address: embassy_net::Ipv4Cidr::new(embassy_net::Ipv4Address::new(192, 168, 10, 2), 24),
        dns_servers: Vec::new(),
        gateway: Some(embassy_net::Ipv4Address::new(192, 168, 10, 1)),
    });

    info!("Config and spawn net stack");
    // Generate random seed
    let mut rng = Rng::new(p.RNG, Irqs);
    let mut seed = [0; 8];
    unwrap!(rng.async_fill_bytes(&mut seed).await);
    let seed = u64::from_le_bytes(seed);

    // Init network stack
    static STACK: StaticCell<Stack<Device<'static, MTU>>> = StaticCell::new();
    static RESOURCES: StaticCell<StackResources<2>> = StaticCell::new();
    let stack = &*STACK.init(Stack::new(
        device,
        config,
        RESOURCES.init(StackResources::<2>::new()),
        seed,
    ));

    unwrap!(spawner.spawn(net_task(stack)));

    // And now we can use it!
    info!("Start application!");

    let mut rx_buffer = [0; 4096];
    let mut tx_buffer = [0; 4096];
    let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
    socket.set_timeout(Some(embassy_time::Duration::from_secs(10)));
    socket.set_keep_alive(Some(embassy_time::Duration::from_secs(10)));

    loop {
        info!("Listening on TCP:1234...");
        if let Err(e) = socket.accept(1234).await {
            error!("accept error: {:?}", e);
            continue;
        }

        info!("Received connection from {:?}", socket.remote_endpoint());

        loop {
            // TCP to USART
            let mut vec: Vec<u8, BUFFER_SIZE_TCP_TO_USART> = Vec::new();
            vec.resize(BUFFER_SIZE_TCP_TO_USART, 0).unwrap();
            if socket.can_recv() {
                if let Either::First(read_ret) = select(socket.read(&mut vec), Timer::after_millis(10)).await {
                    match read_ret {
                        Ok(0) => {
                            error!("read EOF");
                            break;
                        }
                        Ok(len) => {
                            vec.resize(len, 0).unwrap();
                            CHANNEL_TCP_TO_USART.send(vec).await;
                        }
                        Err(e) => {
                            error!("read error: {:?}", e);
                            break;
                        }
                    }
                };
            }

            // USART to TCP
            if socket.may_send() {
                if let Either::First(vec) = select(CHANNEL_USART_TO_TCP.receive(), Timer::after_millis(10)).await {
                    match socket.write_all(&vec).await {
                        Ok(()) => {
                            Timer::after_millis(5).await;
                        }
                        Err(e) => {
                            error!("write error: {:?}", e);
                            break;
                        }
                    };
                }
            }

            Timer::after_millis(5).await;
        }
    }
}
