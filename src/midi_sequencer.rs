use log::*;
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

use midly;

use crate::time_controller::{TimeController, TimeListener, TimeListenerTrait};
#[derive(Debug)]
pub enum MidiEvent {
    NoteOn(u8, u8, u8),
    NoteOff(u8, u8, u8),
    Aftertouch(u8, u8, u8),
    Controller(u8, u8, u8),
    ChannelAftertouch(u8, u8),
    PitchBend(u8, u16),
    ProgramChange(u8, u8),
}
impl MidiEvent {
    pub fn as_raw(&self, _trk_idx: usize) -> Vec<u8> {
        match self {
            MidiEvent::NoteOn(channel, key, pressure) => 
                                vec![
                                    0x90 + channel,
                                    *key,
                                    *pressure,
                                ],
            MidiEvent::NoteOff(channel, key, pressure) => 
                                vec![
                                    0x80 + channel,
                                    *key,
                                    *pressure,
                                ],
            MidiEvent::Controller(channel, control, value) => 
                                vec![
                                    0xb0 + channel,
                                    *control,
                                    *value,
                                ],
            MidiEvent::Aftertouch(channel, key, pressure) => 
                                vec![
                                    0xa0 + channel,
                                    *key,
                                    *pressure,
                                ],
            MidiEvent::ChannelAftertouch(channel, pressure) => 
                                vec![
                                    0xd0 + channel,
                                    *pressure,
                                ],
            MidiEvent::PitchBend(channel, change) => 
                                vec![
                                    0xe0 + channel,
                                    (*change & 0x7f) as u8,
                                    (*change >> 7) as u8,
                                ],
            MidiEvent::ProgramChange(channel, program) => 
                                vec![
                                    0xc0 + channel,
                                    *program,
                                ],
        }
    }
}

pub type RawMidiTuple = (u64, usize, MidiEvent);

enum MidiSequencerCommand {
    Ping,
    SetPosition(i64),
    Play(i64, Option<u16>, Option<Vec<RawMidiTuple>>),
    Scale(u16),
    Stop,
}

enum SequencerState {
    Stopped,
    Playing,
    StartPlaying(i64, Option<u16>, Option<Vec<RawMidiTuple>>),
}

struct MidiSequencerThread {
    out_port: usize,
    control: mpsc::Receiver<MidiSequencerCommand>,
    events: Vec<RawMidiTuple>,
    time_control: TimeController
}
impl MidiSequencerThread {
    fn new(
        control: mpsc::Receiver<MidiSequencerCommand>,
        out_port: usize,
        events: Vec<RawMidiTuple>,
        time_control: TimeController,
    ) -> MidiSequencerThread {
        MidiSequencerThread { out_port, control, events, time_control }
    }
    fn run(&mut self) {
        trace!("Opening connection");
        let midi_out = midir::MidiOutput::new("My Test Output").unwrap();
        let mut conn_out = midi_out.connect(self.out_port, "midir-test").unwrap();
        trace!("Connection opened");
        let mut idx = 0;
        let mut state = SequencerState::Stopped;
        'main: loop {
            state = match state {
                SequencerState::Stopped => {
                    match self.control.recv() {
                        Err(mpsc::RecvError) => break,
                        Ok(MidiSequencerCommand::Ping) => SequencerState::Stopped,
                        Ok(MidiSequencerCommand::Play(pos_us, opt_scaling, opt_events)) => 
                            SequencerState::StartPlaying(pos_us, opt_scaling, opt_events),
                        Ok(MidiSequencerCommand::Scale(new_scaling)) => {
                            self.time_control.set_scaling_1000(new_scaling);
                            SequencerState::Stopped
                        },
                        Ok(MidiSequencerCommand::SetPosition(pos_us)) => {
                            self.time_control.set_pos_us(pos_us);
                            SequencerState::Stopped
                        },
                        Ok(MidiSequencerCommand::Stop) => SequencerState::Stopped,
                    }
                },
                SequencerState::Playing => {
                    match self.control.try_recv() {
                        Err(mpsc::TryRecvError::Disconnected) => break,
                        Err(mpsc::TryRecvError::Empty) => SequencerState::Playing,
                        Ok(MidiSequencerCommand::Ping) => SequencerState::Playing,
                        Ok(MidiSequencerCommand::Play(pos_us, opt_scaling, opt_events)) => 
                            SequencerState::StartPlaying(pos_us, opt_scaling, opt_events),
                        Ok(MidiSequencerCommand::Scale(new_scaling)) => {
                            self.time_control.set_scaling_1000(new_scaling);
                            SequencerState::Playing
                        },
                        Ok(MidiSequencerCommand::SetPosition(pos_us)) => {
                            SequencerState::StartPlaying(pos_us, None, None)
                        },
                        Ok(MidiSequencerCommand::Stop) => {
                            self.time_control.stop();
                            for channel in 0..15 {
                                let msg = [0x0b+channel, 123, 0]; // All Notes Off
                                conn_out.send(&msg).unwrap();
                            }
                            SequencerState::Stopped
                        },
                    }
                },
                SequencerState::StartPlaying(_, _, _) => panic!("StartPlaying should not be reachable here")
            };

            state = match state {
                SequencerState::Stopped => SequencerState::Stopped,
                SequencerState::StartPlaying(pos_us, opt_scaling, opt_events) => {
                    idx = 0;
                    self.time_control.set_pos_us(pos_us as i64);
                    opt_scaling.map(|scale| self.time_control.set_scaling_1000(scale));
                    if let Some(events) = opt_events {
                        self.events = events;
                    }
                    while pos_us >= self.events[idx].0 as i64 {
                        idx += 1;
                        if idx >= self.events.len() {
                            break 'main;
                        }
                    }
                    self.time_control.start();
                    SequencerState::Playing
                },
                SequencerState::Playing => {
                    let pos_us = self.time_control.get_pos_us();
                    while pos_us >= self.events[idx].0 as i64 {
                        let msg = self.events[idx].2.as_raw(self.events[idx].1);
                        if msg.len() > 0 {
                            conn_out.send(&msg).unwrap();
                        }
                        idx += 1;
                        if idx >= self.events.len() {
                            break 'main;
                        }
                    }

                    let next_pos = self.events[idx].0 as i64;
                    let opt_sleep_ms = self.time_control.ms_till_pos(next_pos);
                    if let Some(sleep_ms) = opt_sleep_ms {
                        let sleep_ms = sleep_ms.min(20);
                        trace!("sleep {} ms",sleep_ms);
                        sleep(Duration::from_millis(sleep_ms as u64));
                    }
                    SequencerState::Playing
                }
            }
        }
        conn_out.close();
        trace!("Connection closed");
    }
}

pub struct MidiSequencer {
    time_listener: TimeListener,
    control: mpsc::Sender<MidiSequencerCommand>,
}

impl MidiSequencer {
    pub fn new(out_port: usize, events: Vec<RawMidiTuple>) -> MidiSequencer {
        let (tx, rx) = mpsc::channel();
        let controller = TimeController::new();
        let time_listener = controller.new_listener();
        thread::spawn(move || MidiSequencerThread::new(rx, out_port, events, controller).run());
        MidiSequencer { control: tx, time_listener }
    }
    pub fn get_new_listener(&self) -> TimeListener {
        self.time_listener.clone()
    }
    pub fn set_pos_us(&self, pos_us: i64) {
        self.control.send(MidiSequencerCommand::SetPosition(pos_us)).ok();
    }
    pub fn pos_us(&self) -> i64 {
        self.time_listener.get_pos_us()
    }
    pub fn is_finished(&self) -> bool {
        self.control.send(MidiSequencerCommand::Ping).is_err()
    }
    pub fn play(&self, pos_us: i64, opt_scale: Option<u16>, opt_events: Option<Vec<RawMidiTuple>>) {
        self.control.send(MidiSequencerCommand::Play(pos_us,opt_scale,opt_events)).ok();
    }
    pub fn set_scaling_1000(&self, new_scale: u16) {
        self.control.send(MidiSequencerCommand::Scale(new_scale)).ok();
    }
    pub fn stop(&self) {
        self.control.send(MidiSequencerCommand::Stop).ok();
    }
}
