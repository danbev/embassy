#![macro_use]

use core::default::Default;
use core::marker::PhantomData;
use core::task::Poll;

use embassy::waitqueue::AtomicWaker;
use embassy_hal_common::drop::OnDrop;
use embassy_hal_common::unborrow;
use futures::future::poll_fn;
use sdio_host::{BusWidth, CardCapacity, CardStatus, CurrentState, SDStatus, CID, CSD, OCR, SCR};

use crate::dma::NoDma;
use crate::gpio::sealed::AFType;
use crate::gpio::{Pull, Speed};
use crate::interrupt::{Interrupt, InterruptExt};
use crate::pac::sdmmc::Sdmmc as RegBlock;
use crate::rcc::RccPeripheral;
use crate::time::Hertz;
use crate::{peripherals, Unborrow};

/// The signalling scheme used on the SDMMC bus
#[non_exhaustive]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Signalling {
    SDR12,
    SDR25,
    SDR50,
    SDR104,
    DDR50,
}

impl Default for Signalling {
    fn default() -> Self {
        Signalling::SDR12
    }
}

#[repr(align(4))]
pub struct DataBlock([u8; 512]);

/// Errors
#[non_exhaustive]
#[derive(Debug, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Error {
    Timeout,
    SoftwareTimeout,
    UnsupportedCardVersion,
    UnsupportedCardType,
    Crc,
    DataCrcFail,
    RxOverFlow,
    NoCard,
    BadClock,
    SignalingSwitchFailed,
    PeripheralBusy,
}

/// A SD command
struct Cmd {
    cmd: u8,
    arg: u32,
    resp: Response,
}

#[derive(Clone, Copy, Debug, Default)]
/// SD Card
pub struct Card {
    /// The type of this card
    pub card_type: CardCapacity,
    /// Operation Conditions Register
    pub ocr: OCR,
    /// Relative Card Address
    pub rca: u32,
    /// Card ID
    pub cid: CID,
    /// Card Specific Data
    pub csd: CSD,
    /// SD CARD Configuration Register
    pub scr: SCR,
    /// SD Status
    pub status: SDStatus,
}

impl Card {
    /// Size in bytes
    pub fn size(&self) -> u64 {
        // SDHC / SDXC / SDUC
        u64::from(self.csd.block_count()) * 512
    }
}

#[repr(u8)]
enum PowerCtrl {
    Off = 0b00,
    On = 0b11,
}

#[repr(u32)]
#[allow(dead_code)]
#[allow(non_camel_case_types)]
enum CmdAppOper {
    VOLTAGE_WINDOW_SD = 0x8010_0000,
    HIGH_CAPACITY = 0x4000_0000,
    SDMMC_STD_CAPACITY = 0x0000_0000,
    SDMMC_CHECK_PATTERN = 0x0000_01AA,
    SD_SWITCH_1_8V_CAPACITY = 0x0100_0000,
}

#[derive(Eq, PartialEq, Copy, Clone)]
enum Response {
    None = 0,
    Short = 1,
    Long = 3,
}

cfg_if::cfg_if! {
    if #[cfg(sdmmc_v1)] {
        /// Calculate clock divisor. Returns a SDMMC_CK less than or equal to
        /// `sdmmc_ck` in Hertz.
        ///
        /// Returns `(clk_div, clk_f)`, where `clk_div` is the divisor register
        /// value and `clk_f` is the resulting new clock frequency.
        fn clk_div(ker_ck: Hertz, sdmmc_ck: u32) -> Result<(u8, Hertz), Error> {
            let clk_div = match ker_ck.0 / sdmmc_ck {
                0 | 1 => Ok(0),
                x @ 2..=258 => {
                    Ok((x - 2) as u8)
                }
                _ => Err(Error::BadClock),
            }?;

            // SDIO_CK frequency = SDIOCLK / [CLKDIV + 2]
            let clk_f = Hertz(ker_ck.0 / (clk_div as u32 + 2));
            Ok((clk_div, clk_f))
        }
    } else if #[cfg(sdmmc_v2)] {
        /// Calculate clock divisor. Returns a SDMMC_CK less than or equal to
        /// `sdmmc_ck` in Hertz.
        ///
        /// Returns `(clk_div, clk_f)`, where `clk_div` is the divisor register
        /// value and `clk_f` is the resulting new clock frequency.
        fn clk_div(ker_ck: Hertz, sdmmc_ck: u32) -> Result<(u16, Hertz), Error> {
            match (ker_ck.0 + sdmmc_ck - 1) / sdmmc_ck {
                0 | 1 => Ok((0, ker_ck)),
                x @ 2..=2046 => {
                    let clk_div = ((x + 1) / 2) as u16;
                    let clk = Hertz(ker_ck.0 / (clk_div as u32 * 2));

                    Ok((clk_div, clk))
                }
                _ => Err(Error::BadClock),
            }
        }
    }
}

/// SDMMC configuration
///
/// Default values:
/// data_transfer_timeout: 5_000_000
#[non_exhaustive]
pub struct Config {
    /// The timeout to be set for data transfers, in card bus clock periods
    pub data_transfer_timeout: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_transfer_timeout: 5_000_000,
        }
    }
}

/// Sdmmc device
pub struct Sdmmc<'d, T: Instance, P: Pins<T>, Dma = NoDma> {
    sdmmc: PhantomData<&'d mut T>,
    pins: P,
    irq: T::Interrupt,
    config: Config,
    dma: Dma,
    /// Current clock to card
    clock: Hertz,
    /// Current signalling scheme to card
    signalling: Signalling,
    /// Card
    card: Option<Card>,
}

#[cfg(sdmmc_v1)]
impl<'d, T: Instance, P: Pins<T>, Dma: SdmmcDma<T>> Sdmmc<'d, T, P, Dma> {
    pub fn new(
        _peripheral: impl Unborrow<Target = T> + 'd,
        pins: impl Unborrow<Target = P> + 'd,
        irq: impl Unborrow<Target = T::Interrupt> + 'd,
        config: Config,
        dma: impl Unborrow<Target = Dma> + 'd,
    ) -> Self {
        unborrow!(irq, pins, dma);
        pins.configure();

        T::enable();
        T::reset();

        let inner = T::inner();
        let clock = unsafe { inner.new_inner(T::frequency()) };

        irq.set_handler(Self::on_interrupt);
        irq.unpend();
        irq.enable();

        Self {
            sdmmc: PhantomData,
            pins,
            irq,
            config,
            dma,
            clock,
            signalling: Default::default(),
            card: None,
        }
    }
}

#[cfg(sdmmc_v2)]
impl<'d, T: Instance, P: Pins<T>> Sdmmc<'d, T, P, NoDma> {
    pub fn new(
        _peripheral: impl Unborrow<Target = T> + 'd,
        pins: impl Unborrow<Target = P> + 'd,
        irq: impl Unborrow<Target = T::Interrupt> + 'd,
        config: Config,
    ) -> Self {
        unborrow!(irq, pins);
        pins.configure();

        T::enable();
        T::reset();

        let inner = T::inner();
        let clock = unsafe { inner.new_inner(T::frequency()) };

        irq.set_handler(Self::on_interrupt);
        irq.unpend();
        irq.enable();

        Self {
            sdmmc: PhantomData,
            pins,
            irq,
            config,
            dma: NoDma,
            clock,
            signalling: Default::default(),
            card: None,
        }
    }
}

impl<'d, T: Instance, P: Pins<T>, Dma: SdmmcDma<T>> Sdmmc<'d, T, P, Dma> {
    #[inline(always)]
    pub async fn init_card(&mut self, freq: impl Into<Hertz>) -> Result<(), Error> {
        let inner = T::inner();
        let freq = freq.into();

        inner
            .init_card(
                freq,
                P::BUSWIDTH,
                &mut self.card,
                &mut self.signalling,
                T::frequency(),
                &mut self.clock,
                T::state(),
                self.config.data_transfer_timeout,
                &mut self.dma,
            )
            .await
    }

    #[inline(always)]
    pub async fn read_block(&mut self, block_idx: u32, buffer: &mut DataBlock) -> Result<(), Error> {
        let card_capacity = self.card()?.card_type;
        let inner = T::inner();
        let state = T::state();

        // NOTE(unsafe) DataBlock uses align 4
        let buf = unsafe { &mut *((&mut buffer.0) as *mut [u8; 512] as *mut [u32; 128]) };
        inner
            .read_block(
                block_idx,
                buf,
                card_capacity,
                state,
                self.config.data_transfer_timeout,
                &mut self.dma,
            )
            .await
    }

    pub async fn write_block(&mut self, block_idx: u32, buffer: &DataBlock) -> Result<(), Error> {
        let card = self.card.as_mut().ok_or(Error::NoCard)?;
        let inner = T::inner();
        let state = T::state();

        // NOTE(unsafe) DataBlock uses align 4
        let buf = unsafe { &*((&buffer.0) as *const [u8; 512] as *const [u32; 128]) };
        inner
            .write_block(
                block_idx,
                buf,
                card,
                state,
                self.config.data_transfer_timeout,
                &mut self.dma,
            )
            .await
    }

    /// Get a reference to the initialized card
    ///
    /// # Errors
    ///
    /// Returns Error::NoCard if [`init_card`](#method.init_card)
    /// has not previously succeeded
    #[inline(always)]
    pub fn card(&self) -> Result<&Card, Error> {
        self.card.as_ref().ok_or(Error::NoCard)
    }

    /// Get the current SDMMC bus clock
    pub fn clock(&self) -> Hertz {
        self.clock
    }

    #[inline(always)]
    fn on_interrupt(_: *mut ()) {
        let regs = T::inner();
        let state = T::state();

        regs.data_interrupts(false);
        state.wake();
    }
}

impl<'d, T: Instance, P: Pins<T>, Dma> Drop for Sdmmc<'d, T, P, Dma> {
    fn drop(&mut self) {
        self.irq.disable();
        let inner = T::inner();
        unsafe { inner.on_drop() };
        self.pins.deconfigure();
    }
}

pub struct SdmmcInner(pub(crate) RegBlock);

impl SdmmcInner {
    /// # Safety
    ///
    /// Access to `regs` registers should be exclusive
    unsafe fn new_inner(&self, kernel_clk: Hertz) -> Hertz {
        let regs = self.0;

        // While the SD/SDIO card or eMMC is in identification mode,
        // the SDMMC_CK frequency must be less than 400 kHz.
        let (clkdiv, clock) = unwrap!(clk_div(kernel_clk, 400_000));

        regs.clkcr().write(|w| {
            w.set_widbus(0);
            w.set_clkdiv(clkdiv);
            w.set_pwrsav(false);
            w.set_negedge(false);
            w.set_hwfc_en(true);

            #[cfg(sdmmc_v1)]
            w.set_clken(true);
        });

        // Power off, writen 00: Clock to the card is stopped;
        // D[7:0], CMD, and CK are driven high.
        regs.power().modify(|w| w.set_pwrctrl(PowerCtrl::Off as u8));

        clock
    }

    /// Initializes card (if present) and sets the bus at the
    /// specified frequency.
    #[allow(clippy::too_many_arguments)]
    async fn init_card<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        freq: Hertz,
        bus_width: BusWidth,
        old_card: &mut Option<Card>,
        signalling: &mut Signalling,
        ker_ck: Hertz,
        clock: &mut Hertz,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<(), Error> {
        let regs = self.0;

        // NOTE(unsafe) We have exclusive access to the peripheral
        unsafe {
            regs.power().modify(|w| w.set_pwrctrl(PowerCtrl::On as u8));
            self.cmd(Cmd::idle(), false)?;

            // Check if cards supports CMD8 (with pattern)
            self.cmd(Cmd::hs_send_ext_csd(0x1AA), false)?;
            let r1 = regs.respr(0).read().cardstatus();

            let mut card = if r1 == 0x1AA {
                // Card echoed back the pattern. Must be at least v2
                Card::default()
            } else {
                return Err(Error::UnsupportedCardVersion);
            };

            let ocr = loop {
                // Signal that next command is a app command
                self.cmd(Cmd::app_cmd(0), false)?; // CMD55

                let arg = CmdAppOper::VOLTAGE_WINDOW_SD as u32
                    | CmdAppOper::HIGH_CAPACITY as u32
                    | CmdAppOper::SD_SWITCH_1_8V_CAPACITY as u32;

                // Initialize card
                match self.cmd(Cmd::app_op_cmd(arg), false) {
                    // ACMD41
                    Ok(_) => (),
                    Err(Error::Crc) => (),
                    Err(err) => return Err(err),
                }
                let ocr: OCR = regs.respr(0).read().cardstatus().into();
                if !ocr.is_busy() {
                    // Power up done
                    break ocr;
                }
            };

            if ocr.high_capacity() {
                // Card is SDHC or SDXC or SDUC
                card.card_type = CardCapacity::SDHC;
            } else {
                card.card_type = CardCapacity::SDSC;
            }
            card.ocr = ocr;

            self.cmd(Cmd::all_send_cid(), false)?; // CMD2
            let cid0 = regs.respr(0).read().cardstatus() as u128;
            let cid1 = regs.respr(1).read().cardstatus() as u128;
            let cid2 = regs.respr(2).read().cardstatus() as u128;
            let cid3 = regs.respr(3).read().cardstatus() as u128;
            let cid = (cid0 << 96) | (cid1 << 64) | (cid2 << 32) | (cid3);
            card.cid = cid.into();

            self.cmd(Cmd::send_rel_addr(), false)?;
            card.rca = regs.respr(0).read().cardstatus() >> 16;

            self.cmd(Cmd::send_csd(card.rca << 16), false)?;
            let csd0 = regs.respr(0).read().cardstatus() as u128;
            let csd1 = regs.respr(1).read().cardstatus() as u128;
            let csd2 = regs.respr(2).read().cardstatus() as u128;
            let csd3 = regs.respr(3).read().cardstatus() as u128;
            let csd = (csd0 << 96) | (csd1 << 64) | (csd2 << 32) | (csd3);
            card.csd = csd.into();

            self.select_card(Some(&card))?;

            self.get_scr(&mut card, waker_reg, data_transfer_timeout, dma).await?;

            // Set bus width
            let (width, acmd_arg) = match bus_width {
                BusWidth::Eight => unimplemented!(),
                BusWidth::Four if card.scr.bus_width_four() => (BusWidth::Four, 2),
                _ => (BusWidth::One, 0),
            };
            self.cmd(Cmd::app_cmd(card.rca << 16), false)?;
            self.cmd(Cmd::cmd6(acmd_arg), false)?;

            // CPSMACT and DPSMACT must be 0 to set WIDBUS
            self.wait_idle();

            regs.clkcr().modify(|w| {
                w.set_widbus(match width {
                    BusWidth::One => 0,
                    BusWidth::Four => 1,
                    BusWidth::Eight => 2,
                    _ => panic!("Invalid Bus Width"),
                })
            });

            // Set Clock
            if freq.0 <= 25_000_000 {
                // Final clock frequency
                self.clkcr_set_clkdiv(freq.0, width, ker_ck, clock)?;
            } else {
                // Switch to max clock for SDR12
                self.clkcr_set_clkdiv(25_000_000, width, ker_ck, clock)?;
            }

            // Read status
            self.read_sd_status(&mut card, waker_reg, data_transfer_timeout, dma)
                .await?;

            if freq.0 > 25_000_000 {
                // Switch to SDR25
                *signalling = self
                    .switch_signalling_mode(Signalling::SDR25, waker_reg, data_transfer_timeout, dma)
                    .await?;

                if *signalling == Signalling::SDR25 {
                    // Set final clock frequency
                    self.clkcr_set_clkdiv(freq.0, width, ker_ck, clock)?;

                    if self.read_status(&card)?.state() != CurrentState::Transfer {
                        return Err(Error::SignalingSwitchFailed);
                    }
                }
            }
            // Read status after signalling change
            self.read_sd_status(&mut card, waker_reg, data_transfer_timeout, dma)
                .await?;
            old_card.replace(card);
        }

        Ok(())
    }

    async fn read_block<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        block_idx: u32,
        buffer: &mut [u32; 128],
        capacity: CardCapacity,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<(), Error> {
        // Always read 1 block of 512 bytes
        // SDSC cards are byte addressed hence the blockaddress is in multiples of 512 bytes
        let address = match capacity {
            CardCapacity::SDSC => block_idx * 512,
            _ => block_idx,
        };
        self.cmd(Cmd::set_block_length(512), false)?; // CMD16

        let regs = self.0;
        let on_drop = OnDrop::new(|| unsafe { self.on_drop() });

        unsafe {
            self.prepare_datapath_read(buffer as *mut [u32; 128], 512, 9, data_transfer_timeout, dma);
            self.data_interrupts(true);
        }
        self.cmd(Cmd::read_single_block(address), true)?;

        let res = poll_fn(|cx| {
            waker_reg.register(cx.waker());
            let status = unsafe { regs.star().read() };

            if status.dcrcfail() {
                return Poll::Ready(Err(Error::Crc));
            } else if status.dtimeout() {
                return Poll::Ready(Err(Error::Timeout));
            } else if status.dataend() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await;
        self.clear_interrupt_flags();

        if res.is_ok() {
            on_drop.defuse();
            self.stop_datapath();
        }
        res
    }

    async fn write_block<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        block_idx: u32,
        buffer: &[u32; 128],
        card: &mut Card,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<(), Error> {
        // Always read 1 block of 512 bytes
        // SDSC cards are byte addressed hence the blockaddress is in multiples of 512 bytes
        let address = match card.card_type {
            CardCapacity::SDSC => block_idx * 512,
            _ => block_idx,
        };
        self.cmd(Cmd::set_block_length(512), false)?; // CMD16

        let regs = self.0;
        let on_drop = OnDrop::new(|| unsafe { self.on_drop() });

        unsafe {
            self.prepare_datapath_write(buffer as *const [u32; 128], 512, 9, data_transfer_timeout, dma);
            self.data_interrupts(true);
        }
        self.cmd(Cmd::write_single_block(address), true)?;

        let res = poll_fn(|cx| {
            waker_reg.register(cx.waker());
            let status = unsafe { regs.star().read() };

            if status.dcrcfail() {
                return Poll::Ready(Err(Error::Crc));
            } else if status.dtimeout() {
                return Poll::Ready(Err(Error::Timeout));
            } else if status.dataend() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await;
        self.clear_interrupt_flags();

        match res {
            Ok(_) => {
                on_drop.defuse();
                self.stop_datapath();

                // TODO: Make this configurable
                let mut timeout: u32 = 0x00FF_FFFF;

                // Try to read card status (ACMD13)
                while timeout > 0 {
                    match self.read_sd_status(card, waker_reg, data_transfer_timeout, dma).await {
                        Ok(_) => return Ok(()),
                        Err(Error::Timeout) => (), // Try again
                        Err(e) => return Err(e),
                    }
                    timeout -= 1;
                }
                Err(Error::SoftwareTimeout)
            }
            Err(e) => Err(e),
        }
    }

    /// Data transfer is in progress
    #[inline(always)]
    fn data_active(&self) -> bool {
        let regs = self.0;

        // NOTE(unsafe) Atomic read with no side-effects
        unsafe {
            let status = regs.star().read();
            cfg_if::cfg_if! {
                if #[cfg(sdmmc_v1)] {
                    status.rxact() || status.txact()
                } else if #[cfg(sdmmc_v2)] {
                    status.dpsmact()
                }
            }
        }
    }

    /// Coammand transfer is in progress
    #[inline(always)]
    fn cmd_active(&self) -> bool {
        let regs = self.0;

        // NOTE(unsafe) Atomic read with no side-effects
        unsafe {
            let status = regs.star().read();
            cfg_if::cfg_if! {
                if #[cfg(sdmmc_v1)] {
                    status.cmdact()
                } else if #[cfg(sdmmc_v2)] {
                    status.cpsmact()
                }
            }
        }
    }

    /// Wait idle on CMDACT, RXACT and TXACT (v1) or DOSNACT and CPSMACT (v2)
    #[inline(always)]
    fn wait_idle(&self) {
        while self.data_active() || self.cmd_active() {}
    }

    /// # Safety
    ///
    /// `buffer` must be valid for the whole transfer and word aligned
    unsafe fn prepare_datapath_read<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        buffer: *mut [u32],
        length_bytes: u32,
        block_size: u8,
        data_transfer_timeout: u32,
        #[allow(unused_variables)] dma: &mut Dma,
    ) {
        assert!(block_size <= 14, "Block size up to 2^14 bytes");
        let regs = self.0;

        // Command AND Data state machines must be idle
        self.wait_idle();
        self.clear_interrupt_flags();

        // NOTE(unsafe) We have exclusive access to the regisers

        regs.dtimer().write(|w| w.set_datatime(data_transfer_timeout));
        regs.dlenr().write(|w| w.set_datalength(length_bytes));

        cfg_if::cfg_if! {
            if #[cfg(sdmmc_v1)] {
                let request = dma.request();
                dma.start_read(request, regs.fifor().ptr() as *const u32, buffer, crate::dma::TransferOptions {
                    pburst: crate::dma::Burst::Incr4,
                    flow_ctrl: crate::dma::FlowControl::Peripheral,
                    ..Default::default()
                });
            } else if #[cfg(sdmmc_v2)] {
                regs.idmabase0r().write(|w| w.set_idmabase0(buffer as *mut u32 as u32));
                regs.idmactrlr().modify(|w| w.set_idmaen(true));
            }
        }

        regs.dctrl().modify(|w| {
            w.set_dblocksize(block_size);
            w.set_dtdir(true);
            #[cfg(sdmmc_v1)]
            {
                w.set_dmaen(true);
                w.set_dten(true);
            }
        });
    }

    /// # Safety
    ///
    /// `buffer` must be valid for the whole transfer and word aligned
    unsafe fn prepare_datapath_write<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        buffer: *const [u32],
        length_bytes: u32,
        block_size: u8,
        data_transfer_timeout: u32,
        #[allow(unused_variables)] dma: &mut Dma,
    ) {
        assert!(block_size <= 14, "Block size up to 2^14 bytes");
        let regs = self.0;

        // Command AND Data state machines must be idle
        self.wait_idle();
        self.clear_interrupt_flags();

        // NOTE(unsafe) We have exclusive access to the regisers

        regs.dtimer().write(|w| w.set_datatime(data_transfer_timeout));
        regs.dlenr().write(|w| w.set_datalength(length_bytes));

        cfg_if::cfg_if! {
            if #[cfg(sdmmc_v1)] {
                let request = dma.request();
                dma.start_write(request, buffer, regs.fifor().ptr() as *mut u32, crate::dma::TransferOptions {
                    pburst: crate::dma::Burst::Incr4,
                    flow_ctrl: crate::dma::FlowControl::Peripheral,
                    ..Default::default()
                });
            } else if #[cfg(sdmmc_v2)] {
                regs.idmabase0r().write(|w| w.set_idmabase0(buffer as *const u32 as u32));
                regs.idmactrlr().modify(|w| w.set_idmaen(true));
            }
        }

        regs.dctrl().modify(|w| {
            w.set_dblocksize(block_size);
            w.set_dtdir(false);
        });
    }

    /// Stops the DMA datapath
    fn stop_datapath(&self) {
        let regs = self.0;

        unsafe {
            cfg_if::cfg_if! {
                if #[cfg(sdmmc_v1)] {
                    regs.dctrl().modify(|w| {
                        w.set_dmaen(false);
                        w.set_dten(false);
                    });
                } else if #[cfg(sdmmc_v2)] {
                    regs.idmactrlr().modify(|w| w.set_idmaen(false));
                }
            }
        }
    }

    /// Sets the CLKDIV field in CLKCR. Updates clock field in self
    fn clkcr_set_clkdiv(&self, freq: u32, width: BusWidth, ker_ck: Hertz, clock: &mut Hertz) -> Result<(), Error> {
        let regs = self.0;

        let (clkdiv, new_clock) = clk_div(ker_ck, freq)?;
        // Enforce AHB and SDMMC_CK clock relation. See RM0433 Rev 7
        // Section 55.5.8
        let sdmmc_bus_bandwidth = new_clock.0 * (width as u32);
        assert!(ker_ck.0 > 3 * sdmmc_bus_bandwidth / 32);
        *clock = new_clock;

        // NOTE(unsafe) We have exclusive access to the regblock
        unsafe {
            // CPSMACT and DPSMACT must be 0 to set CLKDIV
            self.wait_idle();
            regs.clkcr().modify(|w| w.set_clkdiv(clkdiv));
        }

        Ok(())
    }

    /// Switch mode using CMD6.
    ///
    /// Attempt to set a new signalling mode. The selected
    /// signalling mode is returned. Expects the current clock
    /// frequency to be > 12.5MHz.
    async fn switch_signalling_mode<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        signalling: Signalling,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<Signalling, Error> {
        // NB PLSS v7_10 4.3.10.4: "the use of SET_BLK_LEN command is not
        // necessary"

        let set_function = 0x8000_0000
            | match signalling {
                // See PLSS v7_10 Table 4-11
                Signalling::DDR50 => 0xFF_FF04,
                Signalling::SDR104 => 0xFF_1F03,
                Signalling::SDR50 => 0xFF_1F02,
                Signalling::SDR25 => 0xFF_FF01,
                Signalling::SDR12 => 0xFF_FF00,
            };

        let mut status = [0u32; 16];

        // Arm `OnDrop` after the buffer, so it will be dropped first
        let regs = self.0;
        let on_drop = OnDrop::new(|| unsafe { self.on_drop() });

        unsafe {
            self.prepare_datapath_read(&mut status as *mut [u32; 16], 64, 6, data_transfer_timeout, dma);
            self.data_interrupts(true);
        }
        self.cmd(Cmd::cmd6(set_function), true)?; // CMD6

        let res = poll_fn(|cx| {
            waker_reg.register(cx.waker());
            let status = unsafe { regs.star().read() };

            if status.dcrcfail() {
                return Poll::Ready(Err(Error::Crc));
            } else if status.dtimeout() {
                return Poll::Ready(Err(Error::Timeout));
            } else if status.dataend() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await;
        self.clear_interrupt_flags();

        // Host is allowed to use the new functions at least 8
        // clocks after the end of the switch command
        // transaction. We know the current clock period is < 80ns,
        // so a total delay of 640ns is required here
        for _ in 0..300 {
            cortex_m::asm::nop();
        }

        match res {
            Ok(_) => {
                on_drop.defuse();
                self.stop_datapath();

                // Function Selection of Function Group 1
                let selection = (u32::from_be(status[4]) >> 24) & 0xF;

                match selection {
                    0 => Ok(Signalling::SDR12),
                    1 => Ok(Signalling::SDR25),
                    2 => Ok(Signalling::SDR50),
                    3 => Ok(Signalling::SDR104),
                    4 => Ok(Signalling::DDR50),
                    _ => Err(Error::UnsupportedCardType),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Query the card status (CMD13, returns R1)
    ///
    fn read_status(&self, card: &Card) -> Result<CardStatus, Error> {
        let regs = self.0;
        let rca = card.rca;

        self.cmd(Cmd::card_status(rca << 16), false)?; // CMD13

        // NOTE(unsafe) Atomic read with no side-effects
        let r1 = unsafe { regs.respr(0).read().cardstatus() };
        Ok(r1.into())
    }

    /// Reads the SD Status (ACMD13)
    async fn read_sd_status<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        card: &mut Card,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<(), Error> {
        let rca = card.rca;
        self.cmd(Cmd::set_block_length(64), false)?; // CMD16
        self.cmd(Cmd::app_cmd(rca << 16), false)?; // APP

        let mut status = [0u32; 16];

        // Arm `OnDrop` after the buffer, so it will be dropped first
        let regs = self.0;
        let on_drop = OnDrop::new(|| unsafe { self.on_drop() });

        unsafe {
            self.prepare_datapath_read(&mut status as *mut [u32; 16], 64, 6, data_transfer_timeout, dma);
            self.data_interrupts(true);
        }
        self.cmd(Cmd::card_status(0), true)?;

        let res = poll_fn(|cx| {
            waker_reg.register(cx.waker());
            let status = unsafe { regs.star().read() };

            if status.dcrcfail() {
                return Poll::Ready(Err(Error::Crc));
            } else if status.dtimeout() {
                return Poll::Ready(Err(Error::Timeout));
            } else if status.dataend() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await;
        self.clear_interrupt_flags();

        if res.is_ok() {
            on_drop.defuse();
            self.stop_datapath();

            for byte in status.iter_mut() {
                *byte = u32::from_be(*byte);
            }
            card.status = status.into();
        }
        res
    }

    /// Select one card and place it into the _Tranfer State_
    ///
    /// If `None` is specifed for `card`, all cards are put back into
    /// _Stand-by State_
    fn select_card(&self, card: Option<&Card>) -> Result<(), Error> {
        // Determine Relative Card Address (RCA) of given card
        let rca = card.map(|c| c.rca << 16).unwrap_or(0);

        let r = self.cmd(Cmd::sel_desel_card(rca), false);
        match (r, rca) {
            (Err(Error::Timeout), 0) => Ok(()),
            _ => r,
        }
    }

    /// Clear flags in interrupt clear register
    #[inline(always)]
    fn clear_interrupt_flags(&self) {
        let regs = self.0;
        // NOTE(unsafe) Atomic write
        unsafe {
            regs.icr().write(|w| {
                w.set_ccrcfailc(true);
                w.set_dcrcfailc(true);
                w.set_ctimeoutc(true);
                w.set_dtimeoutc(true);
                w.set_txunderrc(true);
                w.set_rxoverrc(true);
                w.set_cmdrendc(true);
                w.set_cmdsentc(true);
                w.set_dataendc(true);
                w.set_dbckendc(true);
                w.set_sdioitc(true);

                #[cfg(sdmmc_v2)]
                {
                    w.set_dholdc(true);
                    w.set_dabortc(true);
                    w.set_busyd0endc(true);
                    w.set_ackfailc(true);
                    w.set_acktimeoutc(true);
                    w.set_vswendc(true);
                    w.set_ckstopc(true);
                    w.set_idmatec(true);
                    w.set_idmabtcc(true);
                }
            });
        }
    }

    /// Enables the interrupts for data transfer
    #[inline(always)]
    fn data_interrupts(&self, enable: bool) {
        let regs = self.0;
        // NOTE(unsafe) Atomic write
        unsafe {
            regs.maskr().write(|w| {
                w.set_dcrcfailie(enable);
                w.set_dtimeoutie(enable);
                w.set_dataendie(enable);

                #[cfg(sdmmc_v2)]
                w.set_dabortie(enable);
            });
        }
    }

    async fn get_scr<T: Instance, Dma: SdmmcDma<T>>(
        &self,
        card: &mut Card,
        waker_reg: &AtomicWaker,
        data_transfer_timeout: u32,
        dma: &mut Dma,
    ) -> Result<(), Error> {
        // Read the the 64-bit SCR register
        self.cmd(Cmd::set_block_length(8), false)?; // CMD16
        self.cmd(Cmd::app_cmd(card.rca << 16), false)?;

        let mut scr = [0u32; 2];

        // Arm `OnDrop` after the buffer, so it will be dropped first
        let regs = self.0;
        let on_drop = OnDrop::new(move || unsafe { self.on_drop() });

        unsafe {
            self.prepare_datapath_read(&mut scr as *mut [u32], 8, 3, data_transfer_timeout, dma);
            self.data_interrupts(true);
        }
        self.cmd(Cmd::cmd51(), true)?;

        let res = poll_fn(|cx| {
            waker_reg.register(cx.waker());
            let status = unsafe { regs.star().read() };

            if status.dcrcfail() {
                return Poll::Ready(Err(Error::Crc));
            } else if status.dtimeout() {
                return Poll::Ready(Err(Error::Timeout));
            } else if status.dataend() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        })
        .await;
        self.clear_interrupt_flags();

        if res.is_ok() {
            on_drop.defuse();
            self.stop_datapath();

            unsafe {
                let scr_bytes = &*(&scr as *const [u32; 2] as *const [u8; 8]);
                card.scr = SCR(u64::from_be_bytes(*scr_bytes));
            }
        }
        res
    }

    /// Send command to card
    #[allow(unused_variables)]
    fn cmd(&self, cmd: Cmd, data: bool) -> Result<(), Error> {
        let regs = self.0;

        self.clear_interrupt_flags();
        // NOTE(safety) Atomic operations
        unsafe {
            // CP state machine must be idle
            while self.cmd_active() {}

            // Command arg
            regs.argr().write(|w| w.set_cmdarg(cmd.arg));

            // Command index and start CP State Machine
            regs.cmdr().write(|w| {
                w.set_waitint(false);
                w.set_waitresp(cmd.resp as u8);
                w.set_cmdindex(cmd.cmd);
                w.set_cpsmen(true);

                #[cfg(sdmmc_v2)]
                {
                    // Special mode in CP State Machine
                    // CMD12: Stop Transmission
                    let cpsm_stop_transmission = cmd.cmd == 12;
                    w.set_cmdstop(cpsm_stop_transmission);
                    w.set_cmdtrans(data);
                }
            });

            let mut status;
            if cmd.resp == Response::None {
                // Wait for CMDSENT or a timeout
                while {
                    status = regs.star().read();
                    !(status.ctimeout() || status.cmdsent())
                } {}
            } else {
                // Wait for CMDREND or CCRCFAIL or a timeout
                while {
                    status = regs.star().read();
                    !(status.ctimeout() || status.cmdrend() || status.ccrcfail())
                } {}
            }

            if status.ctimeout() {
                return Err(Error::Timeout);
            } else if status.ccrcfail() {
                return Err(Error::Crc);
            }
            Ok(())
        }
    }

    /// # Safety
    ///
    /// Ensure that `regs` has exclusive access to the regblocks
    unsafe fn on_drop(&self) {
        let regs = self.0;
        if self.data_active() {
            self.clear_interrupt_flags();
            // Send abort
            // CP state machine must be idle
            while self.cmd_active() {}

            // Command arg
            regs.argr().write(|w| w.set_cmdarg(0));

            // Command index and start CP State Machine
            regs.cmdr().write(|w| {
                w.set_waitint(false);
                w.set_waitresp(Response::Short as u8);
                w.set_cmdindex(12);
                w.set_cpsmen(true);

                #[cfg(sdmmc_v2)]
                {
                    w.set_cmdstop(true);
                    w.set_cmdtrans(false);
                }
            });

            // Wait for the abort
            while self.data_active() {}
        }
        self.data_interrupts(false);
        self.clear_interrupt_flags();
        self.stop_datapath();
    }
}

/// SD card Commands
impl Cmd {
    const fn new(cmd: u8, arg: u32, resp: Response) -> Cmd {
        Cmd { cmd, arg, resp }
    }

    /// CMD0: Idle
    const fn idle() -> Cmd {
        Cmd::new(0, 0, Response::None)
    }

    /// CMD2: Send CID
    const fn all_send_cid() -> Cmd {
        Cmd::new(2, 0, Response::Long)
    }

    /// CMD3: Send Relative Address
    const fn send_rel_addr() -> Cmd {
        Cmd::new(3, 0, Response::Short)
    }

    /// CMD6: Switch Function Command
    /// ACMD6: Bus Width
    const fn cmd6(arg: u32) -> Cmd {
        Cmd::new(6, arg, Response::Short)
    }

    /// CMD7: Select one card and put it into the _Tranfer State_
    const fn sel_desel_card(rca: u32) -> Cmd {
        Cmd::new(7, rca, Response::Short)
    }

    /// CMD8:
    const fn hs_send_ext_csd(arg: u32) -> Cmd {
        Cmd::new(8, arg, Response::Short)
    }

    /// CMD9:
    const fn send_csd(rca: u32) -> Cmd {
        Cmd::new(9, rca, Response::Long)
    }

    /// CMD12:
    //const fn stop_transmission() -> Cmd {
    //    Cmd::new(12, 0, Response::Short)
    //}

    /// CMD13: Ask card to send status register
    /// ACMD13: SD Status
    const fn card_status(rca: u32) -> Cmd {
        Cmd::new(13, rca, Response::Short)
    }

    /// CMD16:
    const fn set_block_length(blocklen: u32) -> Cmd {
        Cmd::new(16, blocklen, Response::Short)
    }

    /// CMD17: Block Read
    const fn read_single_block(addr: u32) -> Cmd {
        Cmd::new(17, addr, Response::Short)
    }

    /// CMD18: Multiple Block Read
    //const fn read_multiple_blocks(addr: u32) -> Cmd {
    //    Cmd::new(18, addr, Response::Short)
    //}

    /// CMD24: Block Write
    const fn write_single_block(addr: u32) -> Cmd {
        Cmd::new(24, addr, Response::Short)
    }

    const fn app_op_cmd(arg: u32) -> Cmd {
        Cmd::new(41, arg, Response::Short)
    }

    const fn cmd51() -> Cmd {
        Cmd::new(51, 0, Response::Short)
    }

    /// App Command. Indicates that next command will be a app command
    const fn app_cmd(rca: u32) -> Cmd {
        Cmd::new(55, rca, Response::Short)
    }
}

//////////////////////////////////////////////////////

pub(crate) mod sealed {
    use super::*;

    pub trait Instance {
        type Interrupt: Interrupt;

        fn inner() -> SdmmcInner;
        fn state() -> &'static AtomicWaker;
    }

    pub trait Pins<T: Instance> {}
}

pub trait Instance: sealed::Instance + RccPeripheral + 'static {}
pin_trait!(CkPin, Instance);
pin_trait!(CmdPin, Instance);
pin_trait!(D0Pin, Instance);
pin_trait!(D1Pin, Instance);
pin_trait!(D2Pin, Instance);
pin_trait!(D3Pin, Instance);
pin_trait!(D4Pin, Instance);
pin_trait!(D5Pin, Instance);
pin_trait!(D6Pin, Instance);
pin_trait!(D7Pin, Instance);

cfg_if::cfg_if! {
    if #[cfg(sdmmc_v1)] {
        dma_trait!(SdmmcDma, Instance);
    } else if #[cfg(sdmmc_v2)] {
        // SDMMCv2 uses internal DMA
        pub trait SdmmcDma<T: Instance> {}
        impl<T: Instance> SdmmcDma<T> for NoDma {}
    }
}

pub trait Pins<T: Instance>: sealed::Pins<T> + 'static {
    const BUSWIDTH: BusWidth;

    fn configure(&mut self);
    fn deconfigure(&mut self);
}

impl<T, CLK, CMD, D0, D1, D2, D3> sealed::Pins<T> for (CLK, CMD, D0, D1, D2, D3)
where
    T: Instance,
    CLK: CkPin<T>,
    CMD: CmdPin<T>,
    D0: D0Pin<T>,
    D1: D1Pin<T>,
    D2: D2Pin<T>,
    D3: D3Pin<T>,
{
}

impl<T, CLK, CMD, D0> sealed::Pins<T> for (CLK, CMD, D0)
where
    T: Instance,
    CLK: CkPin<T>,
    CMD: CmdPin<T>,
    D0: D0Pin<T>,
{
}

impl<T, CLK, CMD, D0, D1, D2, D3> Pins<T> for (CLK, CMD, D0, D1, D2, D3)
where
    T: Instance,
    CLK: CkPin<T>,
    CMD: CmdPin<T>,
    D0: D0Pin<T>,
    D1: D1Pin<T>,
    D2: D2Pin<T>,
    D3: D3Pin<T>,
{
    const BUSWIDTH: BusWidth = BusWidth::Four;

    fn configure(&mut self) {
        let (clk_pin, cmd_pin, d0_pin, d1_pin, d2_pin, d3_pin) = self;

        critical_section::with(|_| unsafe {
            clk_pin.set_as_af_pull(clk_pin.af_num(), AFType::OutputPushPull, Pull::None);
            cmd_pin.set_as_af_pull(cmd_pin.af_num(), AFType::OutputPushPull, Pull::Up);
            d0_pin.set_as_af_pull(d0_pin.af_num(), AFType::OutputPushPull, Pull::Up);
            d1_pin.set_as_af_pull(d1_pin.af_num(), AFType::OutputPushPull, Pull::Up);
            d2_pin.set_as_af_pull(d2_pin.af_num(), AFType::OutputPushPull, Pull::Up);
            d3_pin.set_as_af_pull(d3_pin.af_num(), AFType::OutputPushPull, Pull::Up);

            clk_pin.set_speed(Speed::VeryHigh);
            cmd_pin.set_speed(Speed::VeryHigh);
            d0_pin.set_speed(Speed::VeryHigh);
            d1_pin.set_speed(Speed::VeryHigh);
            d2_pin.set_speed(Speed::VeryHigh);
            d3_pin.set_speed(Speed::VeryHigh);
        });
    }

    fn deconfigure(&mut self) {
        let (clk_pin, cmd_pin, d0_pin, d1_pin, d2_pin, d3_pin) = self;

        critical_section::with(|_| unsafe {
            clk_pin.set_as_disconnected();
            cmd_pin.set_as_disconnected();
            d0_pin.set_as_disconnected();
            d1_pin.set_as_disconnected();
            d2_pin.set_as_disconnected();
            d3_pin.set_as_disconnected();
        });
    }
}

impl<T, CLK, CMD, D0> Pins<T> for (CLK, CMD, D0)
where
    T: Instance,
    CLK: CkPin<T>,
    CMD: CmdPin<T>,
    D0: D0Pin<T>,
{
    const BUSWIDTH: BusWidth = BusWidth::One;

    fn configure(&mut self) {
        let (clk_pin, cmd_pin, d0_pin) = self;

        critical_section::with(|_| unsafe {
            clk_pin.set_as_af_pull(clk_pin.af_num(), AFType::OutputPushPull, Pull::None);
            cmd_pin.set_as_af_pull(cmd_pin.af_num(), AFType::OutputPushPull, Pull::Up);
            d0_pin.set_as_af_pull(d0_pin.af_num(), AFType::OutputPushPull, Pull::Up);

            clk_pin.set_speed(Speed::VeryHigh);
            cmd_pin.set_speed(Speed::VeryHigh);
            d0_pin.set_speed(Speed::VeryHigh);
        });
    }

    fn deconfigure(&mut self) {
        let (clk_pin, cmd_pin, d0_pin) = self;

        critical_section::with(|_| unsafe {
            clk_pin.set_as_disconnected();
            cmd_pin.set_as_disconnected();
            d0_pin.set_as_disconnected();
        });
    }
}

foreach_peripheral!(
    (sdmmc, $inst:ident) => {
        impl sealed::Instance for peripherals::$inst {
            type Interrupt = crate::interrupt::$inst;

            fn inner() -> SdmmcInner {
                const INNER: SdmmcInner = SdmmcInner(crate::pac::$inst);
                INNER
            }

            fn state() -> &'static ::embassy::waitqueue::AtomicWaker {
                static WAKER: ::embassy::waitqueue::AtomicWaker = ::embassy::waitqueue::AtomicWaker::new();
                &WAKER
            }
        }

        impl Instance for peripherals::$inst {}
    };
);

#[cfg(feature = "sdmmc-rs")]
mod sdmmc_rs {
    use core::future::Future;

    use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx};

    use super::*;

    impl<'d, T: Instance, P: Pins<T>> BlockDevice for Sdmmc<'d, T, P> {
        type Error = Error;
        type ReadFuture<'a>
        where
            Self: 'a,
        = impl Future<Output = Result<(), Self::Error>> + 'a;
        type WriteFuture<'a>
        where
            Self: 'a,
        = impl Future<Output = Result<(), Self::Error>> + 'a;

        fn read<'a>(
            &'a mut self,
            blocks: &'a mut [Block],
            start_block_idx: BlockIdx,
            _reason: &str,
        ) -> Self::ReadFuture<'a> {
            async move {
                let card_capacity = self.card()?.card_type;
                let inner = T::inner();
                let state = T::state();
                let mut address = start_block_idx.0;

                for block in blocks.iter_mut() {
                    let block: &mut [u8; 512] = &mut block.contents;

                    // NOTE(unsafe) Block uses align(4)
                    let buf = unsafe { &mut *(block as *mut [u8; 512] as *mut [u32; 128]) };
                    inner
                        .read_block(address, buf, card_capacity, state, self.config.data_transfer_timeout)
                        .await?;
                    address += 1;
                }
                Ok(())
            }
        }

        fn write<'a>(&'a mut self, blocks: &'a [Block], start_block_idx: BlockIdx) -> Self::WriteFuture<'a> {
            async move {
                let card = self.card.as_mut().ok_or(Error::NoCard)?;
                let inner = T::inner();
                let state = T::state();
                let mut address = start_block_idx.0;

                for block in blocks.iter() {
                    let block: &[u8; 512] = &block.contents;

                    // NOTE(unsafe) DataBlock uses align 4
                    let buf = unsafe { &*(block as *const [u8; 512] as *const [u32; 128]) };
                    inner
                        .write_block(address, buf, card, state, self.config.data_transfer_timeout)
                        .await?;
                    address += 1;
                }
                Ok(())
            }
        }

        fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
            let card = self.card()?;
            let count = card.csd.block_count();
            Ok(BlockCount(count))
        }
    }
}
