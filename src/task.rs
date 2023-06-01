use async_std::{fs::File, prelude::*};
use srmw::*;
use std::{
    io::SeekFrom,
    sync::{atomic::AtomicBool, Arc},
    time::Instant,
};

use crate::flash::{FlashPhase, FlashStatus};

#[derive(derive_new::new)]
pub struct Task<'a> {
    image: File,

    #[new(default)]
    pub writer: MultiWriter<File>,

    pub sender: &'a glib::Sender<FlashStatus>,

    #[new(value = "125")]
    pub millis_between: u64,

    pub is_running: Arc<AtomicBool>,

    check: bool,
}

impl<'a> Task<'a> {
    /// Performs the asynchronous USB device flashing.
    pub async fn process(mut self, buf: &mut [u8]) -> Result<(), ()> {
        self.copy(buf).await?;

        if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(());
        }

        if self.check {
            self.seek().await?;

            if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }

            self.validate(buf).await?;

            if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }
        }

        self.sender
            .send(FlashStatus::Done(None))
            .expect("Concurrency Issues");

        Ok(())
    }

    pub fn subscribe(&mut self, file: File) {
        self.writer.insert(file);
    }

    async fn copy(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        let size = self.image.metadata().await.unwrap().len();

        let mut stream = self.writer.copy(&mut self.image, buf);
        let mut total = 0;
        let mut last = Instant::now();

        while let Some(event) = stream.next().await {
            if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }
            if let Err(()) = match event {
                CopyEvent::Progress(written) => {
                    total += written as u64;
                    let now = Instant::now();
                    if now.duration_since(last).as_millis() > self.millis_between as u128 {
                        last = now;
                        self.sender
                            .send(FlashStatus::Active(
                                FlashPhase::Copy,
                                (total as f64) / (size as f64),
                            ))
                            .expect("Concurrency Issues");
                    }
                    Ok(())
                }
                CopyEvent::Failure(_, why) => {
                    self.sender
                        .send(FlashStatus::Done(Some(why.to_string())))
                        .expect("Concurrency Issues");
                    Err(())
                }
                CopyEvent::SourceFailure(why) => {
                    self.sender
                        .send(FlashStatus::Done(Some(why.to_string())))
                        .expect("Concurrency Issues");
                    Err(())
                }
                CopyEvent::NoWriters => {
                    self.sender
                        .send(FlashStatus::Done(Some("No writers left".to_owned())))
                        .expect("Concurrency Issues");
                    Err(())
                }
            } {
                return Err(());
            }
        }

        Ok(())
    }

    async fn seek(&mut self) -> Result<(), ()> {
        self.sender
            .send(FlashStatus::Active(FlashPhase::Read, 0.0))
            .expect("Concurrency Issues");

        self.image.seek(SeekFrom::Start(0)).await.map_err(|_| ())?;

        let mut stream = self.writer.seek(SeekFrom::Start(0));
        if let Some((_, why)) = stream.next().await {
            self.sender
                .send(FlashStatus::Done(Some(why.to_string())))
                .expect("Concurrency Issues");
            return Err(());
        }

        Ok(())
    }

    async fn validate(&mut self, buf: &mut [u8]) -> Result<(), ()> {
        let size = self.image.metadata().await.unwrap().len();
        self.sender
            .send(FlashStatus::Active(FlashPhase::Validate, 0.0))
            .expect("Concurrency Issues");

        let copy_bufs = &mut Vec::new();
        let mut total = 0;
        let mut stream = self.writer.validate(&mut self.image, buf, copy_bufs);

        while let Some(event) = stream.next().await {
            if !self.is_running.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }
            if let Err(()) = match event {
                ValidationEvent::Progress(written) => {
                    total += written as u64;
                    self.sender
                        .send(FlashStatus::Active(
                            FlashPhase::Validate,
                            (total as f64) / (size as f64),
                        ))
                        .expect("Concurrency Issues");
                    Ok(())
                }
                ValidationEvent::Failure(_, why) => {
                    self.sender
                        .send(FlashStatus::Done(Some(why.to_string())))
                        .expect("Concurrency Issues");
                    Err(())
                }
                ValidationEvent::SourceFailure(why) => {
                    self.sender
                        .send(FlashStatus::Done(Some(why.to_string())))
                        .expect("Concurrency Issues");
                    Err(())
                }
                ValidationEvent::NoWriters => {
                    self.sender
                        .send(FlashStatus::Done(Some("No writers left".to_owned())))
                        .expect("Concurrency Issues");
                    Err(())
                }
            } {
                return Err(());
            }
        }

        Ok(())
    }
}
