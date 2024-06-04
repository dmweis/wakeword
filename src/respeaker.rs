//! Main documentation at <https://wiki.seeedstudio.com/ReSpeaker-USB-Mic-Array/>
//!
//!
//! Code based on <https://github.com/respeaker/pixel_ring/blob/master/pixel_ring/usb_pixel_ring_v2.py>
//! and <https://github.com/respeaker/usb_4_mic_array/blob/master/tuning.py>

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::{thread, time::Duration};

use anyhow::Result;
use rusb::{Context, DeviceHandle, UsbContext};
use tracing::{error, info, warn};

fn find_usb_device(vid: u16, pid: u16) -> Result<Option<PixelRing<Context>>> {
    let context = Context::new()?;
    let devices = context.devices()?;
    for device in devices.iter() {
        let device_desc = device.device_descriptor()?;
        if device_desc.vendor_id() == vid && device_desc.product_id() == pid {
            let handle = device.open()?;
            return Ok(Some(PixelRing::new(handle)));
        }
    }
    Ok(None)
}

struct PixelRing<T: UsbContext> {
    dev: DeviceHandle<T>,
    timeout: Duration,
}

impl<T: UsbContext> PixelRing<T> {
    const TIMEOUT: Duration = Duration::from_secs(8);

    fn new(dev: DeviceHandle<T>) -> Self {
        PixelRing {
            dev,
            timeout: Self::TIMEOUT,
        }
    }

    /// mono mode, set all RGB LED to a single color, for example Red(0xFF0000), Green(0x00FF00)， Blue(0x0000FF)
    fn mono(&mut self, color: u32) -> Result<()> {
        let data = [
            ((color >> 16) & 0xFF) as u8,
            ((color >> 8) & 0xFF) as u8,
            (color & 0xFF) as u8,
            0,
        ];
        self.write(1, &data)
    }

    /// no light
    fn off(&mut self) -> Result<()> {
        self.mono(0)
    }

    /// trace mode, LEDs changing depends on VAD and DOA
    #[allow(unused)]
    fn trace(&mut self) -> Result<()> {
        self.write(0, &[0])
    }

    /// mono mode, set all RGB LED to a single color, for example Red(0xFF0000), Green(0x00FF00)， Blue(0x0000FF)
    #[allow(unused)]
    fn set_color(&mut self, r: u8, g: u8, b: u8) -> Result<()> {
        self.write(1, &[r, g, b, 0])
    }

    /// listen mode, similar with trace mode, but not turn LEDs off
    fn listen(&mut self) -> Result<()> {
        self.write(2, &[0])
    }

    /// speak mode
    #[allow(unused)]
    fn speak(&mut self) -> Result<()> {
        self.write(3, &[0])
    }

    /// wait mode
    fn think(&mut self) -> Result<()> {
        self.write(4, &[0])
    }

    /// spin mode
    #[allow(unused)]
    fn spin(&mut self) -> Result<()> {
        self.write(5, &[0])
    }

    /// custom mode, set each LED to its own color
    #[allow(unused)]
    fn show(&mut self, data: &[u8]) -> Result<()> {
        self.write(6, data)
    }

    /// set brightness, range: 0x00~0x1F
    #[allow(unused)]
    fn set_brightness(&mut self, brightness: u8) -> Result<()> {
        self.write(0x20, &[brightness])
    }

    /// set color palette, for example, pixel_ring.set_color_palette(0xff0000, 0x00ff00) together with pixel_ring.think()
    #[allow(unused)]
    fn set_color_palette(&mut self, a: u32, b: u32) -> Result<()> {
        let data = [
            ((a >> 16) & 0xFF) as u8,
            ((a >> 8) & 0xFF) as u8,
            (a & 0xFF) as u8,
            0,
            ((b >> 16) & 0xFF) as u8,
            ((b >> 8) & 0xFF) as u8,
            (b & 0xFF) as u8,
            0,
        ];
        self.write(0x21, &data)
    }

    /// set center LED: 0 - off, 1 - on, else - depends on VAD
    #[allow(unused)]
    fn set_vad_led(&mut self, state: u8) -> Result<()> {
        self.write(0x22, &[state])
    }

    /// show volume, range: 0 ~ 12
    #[allow(unused)]
    fn set_volume(&mut self, volume: u8) -> Result<()> {
        self.write(0x23, &[volume])
    }

    fn write(&mut self, cmd: u8, data: &[u8]) -> Result<()> {
        self.dev.write_control(
            rusb::request_type(
                rusb::Direction::Out,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            0,
            cmd as u16,
            0x1C,
            data,
            self.timeout,
        )?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.dev.release_interface(0)?;
        Ok(())
    }

    /// DOA angle. Current value. Orientation depends on build configuration.
    fn read_direction(&self) -> Result<i32> {
        let id = 21; // ID for DOAANGLE
        let cmd = 0x80 | 0x40; // Command for reading int
        let length = 8; // Length of data to read

        let mut buf = vec![0; length];

        self.dev.read_control(
            rusb::request_type(
                rusb::Direction::In,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            0,
            cmd,
            id,
            &mut buf,
            self.timeout,
        )?;

        let response: (i32, i32) = bincode::deserialize(&buf)?;
        let result = response.0;

        Ok(result)
    }
}

const VENDOR_ID: u16 = 0x2886;
const PRODUCE_ID: u16 = 0x0018;

#[allow(unused)]
const RED: u32 = 0xFF0000;
#[allow(unused)]
const GREEN: u32 = 0x00FF00;
#[allow(unused)]
const BLUE: u32 = 0x0000FF;

#[allow(unused)]
const BRIGHT_PATTERN_COLOR: u32 = 0x00CAFF;
#[allow(unused)]
const DARK_PATTERN_COLOR: u32 = 0x31C4F3;

enum SpeakerCommand {
    Off,
    Listen,
    Think,
    ReadDirection(SyncSender<i32>),
}

#[derive(Debug, Clone)]
pub struct ReSpeakerCommander {
    sender: SyncSender<SpeakerCommand>,
}

impl ReSpeakerCommander {
    /// Create dummy instance
    pub fn dummy() -> Self {
        warn!("Using dummy ReSpeakerCommander");
        let (sender, _receiver) = sync_channel(10);
        ReSpeakerCommander { sender }
    }

    pub fn off(&self) {
        _ = self.sender.try_send(SpeakerCommand::Off);
    }

    pub fn listen(&self) {
        _ = self.sender.try_send(SpeakerCommand::Listen);
    }

    #[allow(unused)]
    pub fn think(&self) {
        _ = self.sender.try_send(SpeakerCommand::Think);
    }

    #[allow(unused)]
    pub fn read_direction(&self) -> Result<i32> {
        let (sender, receiver) = sync_channel(1);
        self.sender
            .try_send(SpeakerCommand::ReadDirection(sender))?;
        Ok(receiver.recv()?)
    }
}

pub fn start_respeaker_loop() -> ReSpeakerCommander {
    info!("Starting ReSpeaker loop");
    let (sender, receiver) = sync_channel(10);
    thread::spawn(move || respeaker_loop(receiver));

    ReSpeakerCommander { sender }
}

fn respeaker_loop(mut command_receiver: Receiver<SpeakerCommand>) {
    while let Err(err) = run_respeaker(&mut command_receiver) {
        error!("ReSpeaker loop failed with err: {:?}", err);
    }
    info!("Exiting ReSpeaker loop");
}

fn run_respeaker(command_receiver: &mut Receiver<SpeakerCommand>) -> Result<()> {
    if let Some(mut pixel_ring) = find_usb_device(VENDOR_ID, PRODUCE_ID)? {
        info!("Found ReSpeaker USB device. Starting loop");

        // leave default for now
        // pixel_ring.set_color_palette(BRIGHT_PATTERN_COLOR, DARK_PATTERN_COLOR)?;
        pixel_ring.off()?;

        while let Ok(message) = command_receiver.recv() {
            match message {
                SpeakerCommand::Off => pixel_ring.off()?,
                SpeakerCommand::Listen => pixel_ring.listen()?,
                SpeakerCommand::Think => pixel_ring.think()?,
                SpeakerCommand::ReadDirection(response_sender) => {
                    let direction = pixel_ring.read_direction()?;
                    // ignore error here because we don't care if caller is still alive
                    _ = response_sender.send(direction);
                }
            }
        }
        pixel_ring.close()?;
        info!("ReSpeaker command channel closed")
    } else {
        error!("Haven't found ReSpeaker. Waiting");
        std::thread::sleep(Duration::from_millis(500));
    }

    Ok(())
}
