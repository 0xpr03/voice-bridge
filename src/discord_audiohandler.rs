//! COPIED FROM tsclientlib https://github.com/ReSpeak/tsclientlib/blob/e4d2baa8aaee5cd793a982e2805d7baf46b715b9/tsclientlib/src/audio.rs
//! Copyright by their respective owners. 
//! Adopted to allow usage with non-ts packages.
//!
//! Handle receiving audio.
//!
//! The [`AudioHandler`] collects all incoming audio packets and queues them per
//! client. It decodes the audio, handles out-of-order packets and missing
//! packets. It automatically adjusts the queue length based on the jitter of
//! incoming packets.

use std::cmp::Reverse;
use std::collections::{HashMap, VecDeque};
use std::convert::TryInto;
use std::fmt::Debug;
use std::hash::Hash;

use audiopus::coder::Decoder;
use audiopus::{packet, Channels, SampleRate};
use slog::{Logger, debug, info, o, trace, warn};
use tsclientlib::audio::Error;

use crate::ClientId;

const SAMPLE_RATE: SampleRate = SampleRate::Hz48000;
const CHANNELS: Channels = Channels::Stereo;
const CHANNEL_NUM: usize = 2;
/// If this amount of packets is lost consecutively, we assume the stream stopped.
const MAX_PACKET_LOSSES: usize = 3;
/// Store the buffer sizes for the last `LAST_BUFFER_SIZE_COUNT` packets.
const LAST_BUFFER_SIZE_COUNT: u8 = 255;
/// The amount of samples to maximally buffer. Equivalent to 0.5 s.
const MAX_BUFFER_SIZE: usize = 48_000 / 2;
/// Maximum number of packets in the queue.
const MAX_BUFFER_PACKETS: usize = 50;
/// Buffer for maximal 0.5 s without playing anything.
const MAX_BUFFER_TIME: usize = 48_000 / 2;
/// Duplicate or remove every `step` sample when speeding-up.
const SPEED_CHANGE_STEPS: usize = 100;
/// The usual amount of samples in a frame.
///
/// Use 48 kHz, 20 ms frames (50 per second) and mono data (1 channel).
/// This means 1920 samples and 7.5 kiB.
const USUAL_FRAME_SIZE: usize = 48000 / 50;

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Debug)]
struct SlidingWindowMinimum<T: Copy + Default + Ord> {
	/// How long a value stays in the sliding window.
	size: u8,
	/// This is a sliding window minimum, it contains
	/// `(insertion time, value)`.
	///
	/// When we insert a value, we can remove all bigger sample counts,
	/// thus the queue always stays sorted with the minimum at the front
	/// and the maximum at the back (latest entry).
	///
	/// Provides amortized O(1) minimum.
	/// Source: https://people.cs.uct.ac.za/~ksmith/articles/sliding_window_minimum.html#sliding-window-minimum-algorithm
	queue: VecDeque<(u8, T)>,
	/// The current insertion time.
	cur_time: u8,
}

#[derive(Debug)]
struct QueuePacket {
	packet: Vec<u8>,
	samples: usize,
	id: u16,
}

/// A queue for audio packets for one audio stream.
pub struct AudioQueue {
	logger: Logger,
	decoder: Decoder,
	pub volume: f32,
	/// The id of the next packet that should be decoded.
	///
	/// Used to check for packet loss.
	next_id: u16,
	/// If the last packet was a whisper packet.
	whispering: bool,
	packet_buffer: VecDeque<QueuePacket>,
	/// Amount of samples in the `packet_buffer`.
	packet_buffer_samples: usize,
	/// Temporary buffer that contains the samples of one decoded packet.
	decoded_buffer: Vec<f32>,
	/// The current position in the `decoded_buffer`.
	decoded_pos: usize,
	/// The number of samples in the last packet.
	last_packet_samples: usize,
	/// The last `packet_loss_num` packet decodes were a loss.
	packet_loss_num: usize,
	/// The amount of samples to buffer until this queue is ready to play.
	buffering_samples: usize,
	/// The amount of packets in the buffer when a packet was decoded.
	///
	/// Uses the amount of samples in the `packet_buffer` / `USUAL_PACKET_SAMPLES`.
	/// Used to expand or reduce the buffer.
	last_buffer_size_min: SlidingWindowMinimum<u8>,
	last_buffer_size_max: SlidingWindowMinimum<Reverse<u8>>,
	/// Buffered for this duration.
	buffered_for_samples: usize,
}

/// Handles incoming audio, has one [`AudioQueue`] per sending client.
pub struct AudioHandler<Id: Clone + Debug + Eq + Hash + PartialEq = ClientId> {
	logger: Logger,
	queues: HashMap<Id, AudioQueue>,
	/// Buffer this amount of samples for new queues before starting to play.
	///
	/// Updated when a new queue gets added.
	avg_buffer_samples: usize,
}

impl<T: Copy + Default + Ord> SlidingWindowMinimum<T> {
	fn new(size: u8) -> Self { Self { size, queue: Default::default(), cur_time: 0 } }

	fn push(&mut self, value: T) {
		while self.queue.back().map(|(_, s)| *s >= value).unwrap_or_default() {
			self.queue.pop_back();
		}
		let i = self.cur_time;
		self.queue.push_back((i, value));
		while self
			.queue
			.front()
			.map(|(i, _)| self.cur_time.wrapping_sub(*i) >= self.size)
			.unwrap_or_default()
		{
			self.queue.pop_front();
		}
		self.cur_time = self.cur_time.wrapping_add(1);
	}

	fn get_min(&self) -> T { self.queue.front().map(|(_, s)| *s).unwrap_or_default() }
}

impl AudioQueue {
	fn new(logger: Logger, sequence: u16, packet: Vec<u8>) -> Result<Self> {
		let last_packet_samples =
			packet::nb_samples(&packet, SAMPLE_RATE).map_err(Error::GetPacketSample)?;
		if last_packet_samples > MAX_BUFFER_SIZE {
			return Err(Error::TooManySamples);
		}

		let last_packet_samples = last_packet_samples * CHANNEL_NUM;
		let mut res = Self {
			logger,
			decoder: Decoder::new(SAMPLE_RATE, CHANNELS).map_err(Error::CreateDecoder)?,
			volume: 1.0,
			next_id: sequence,
			whispering: false,
			packet_buffer: Default::default(),
			packet_buffer_samples: 0,
			decoded_buffer: Default::default(),
			decoded_pos: 0,
			last_packet_samples,
			packet_loss_num: 0,
			buffering_samples: 0,
			last_buffer_size_min: SlidingWindowMinimum::new(LAST_BUFFER_SIZE_COUNT),
			last_buffer_size_max: SlidingWindowMinimum::<Reverse<u8>>::new(LAST_BUFFER_SIZE_COUNT),
			buffered_for_samples: 0,
		};
		res.add_buffer_size(0);
		res.add_packet(sequence, packet)?;
		Ok(res)
	}

	/// Size is in samples.
	fn add_buffer_size(&mut self, size: usize) {
		if let Ok(size) = (size / USUAL_FRAME_SIZE).try_into() {
			self.last_buffer_size_min.push(size);
			self.last_buffer_size_max.push(Reverse(size));
		} else {
			warn!(self.logger, "Failed to put amount of packets into an u8"; "size" => size);
		}
	}

	/// The approximate deviation of the buffer size.
	fn get_deviation(&self) -> u8 {
		let min = self.last_buffer_size_min.get_min();
		let max = self.last_buffer_size_max.get_min();
		max.0 - min
	}

	fn add_packet(&mut self, sequence: u16, packet: Vec<u8>) -> Result<()> {
		if self.packet_buffer.len() >= MAX_BUFFER_PACKETS {
			return Err(Error::QueueFull);
		}
		let samples;
		if packet.len() <= 1 {
			// End of stream
			samples = 0;
		} else {
			samples = packet::nb_samples(&packet, SAMPLE_RATE)
				.map_err(Error::GetPacketSample)?;
			if samples > MAX_BUFFER_SIZE {
				return Err(Error::TooManySamples);
			}
		}

		let id = sequence;
		let packet = QueuePacket { packet, samples, id };
		if id.wrapping_sub(self.next_id) > MAX_BUFFER_PACKETS as u16 {
			return Err(Error::TooLate { wanted: self.next_id, got: id });
		}

		// Put into first spot where the id is smaller
		let i = self.packet_buffer.len()
			- self
				.packet_buffer
				.iter()
				.enumerate()
				.rev()
				.take_while(|(_, p)| p.id.wrapping_sub(id) <= MAX_BUFFER_PACKETS as u16)
				.count();
		// Check for duplicate packet
		if let Some(p) = self.packet_buffer.get(i) {
			if p.id == packet.id {
				return Err(Error::Duplicate(p.id));
			}
		}

		trace!(self.logger, "Insert packet {} at {}", id, i);
		let last_id = self.packet_buffer.back().map(|p| p.id.wrapping_add(1)).unwrap_or(id);
		if last_id <= id {
			self.buffering_samples = self.buffering_samples.saturating_sub(samples);
			// Reduce buffering counter by lost packets if there are some
			self.buffering_samples = self
				.buffering_samples
				.saturating_sub(usize::from(id - last_id) * self.last_packet_samples);
		}

		self.packet_buffer_samples += packet.samples;
		self.packet_buffer.insert(i, packet);

		Ok(())
	}

	fn decode_packet(&mut self, packet: Option<&QueuePacket>, fec: bool) -> Result<()> {
		debug!(self.logger, "Decoding packet"; "has_packet" => packet.is_some(), "fec" => fec);
		let packet_data;
		let len;
		if let Some(p) = packet {
			packet_data = Some(&p.packet);
			len = p.samples;
			self.whispering = false;
		} else {
			packet_data = None;
			len = self.last_packet_samples;
		}
		self.packet_loss_num += 1;

		self.decoded_buffer.resize(self.decoded_pos + len * CHANNEL_NUM, 0.0);
		let len = self
			.decoder
			.decode_float(packet_data.as_deref(), &mut self.decoded_buffer[self.decoded_pos..], fec)
			.map_err(|e| Error::Decode {
				error: e,
				packet: packet.map(|p| p.packet.to_owned()),
			})?;
		self.last_packet_samples = len;
		self.decoded_buffer.truncate(self.decoded_pos + len * CHANNEL_NUM);
		self.decoded_pos += len * CHANNEL_NUM;

		// Update packet_loss_num
		if packet.is_some() && !fec {
			self.packet_loss_num = 0;
		}

		// Update last_buffer_size
		let mut count = self.packet_buffer_samples;
		if let Some(last) = self.packet_buffer.back() {
			// Lost packets
			trace!(self.logger, "Ids"; "last_id" => last.id,
				"next_id" => self.next_id,
				"first_id" => self.packet_buffer.front().unwrap().id,
				"buffer_len" => self.packet_buffer.len());
			count += (usize::from(last.id.wrapping_sub(self.next_id)) + 1
				- self.packet_buffer.len())
				* self.last_packet_samples;
		}
		self.add_buffer_size(count);

		Ok(())
	}

	/// Decode data and return the requested length of buffered data.
	///
	/// Returns `true` in the second return value when the stream ended,
	/// `false` when it continues normally.
	pub fn get_next_data(&mut self, len: usize) -> Result<(&[f32], bool)> {
		if self.buffering_samples > 0 {
			if self.buffered_for_samples >= MAX_BUFFER_TIME {
				self.buffering_samples = 0;
				self.buffered_for_samples = 0;
				trace!(self.logger, "Buffered for too long";
					"buffered_for_samples" => self.buffered_for_samples,
					"buffering_samples" => self.buffering_samples);
			} else {
				self.buffered_for_samples += len;
				trace!(self.logger, "Buffering";
					"buffered_for_samples" => self.buffered_for_samples,
					"buffering_samples" => self.buffering_samples);
				return Ok((&[], false));
			}
		}
		// Need to refill buffer
		if self.decoded_pos < self.decoded_buffer.len() {
			if self.decoded_pos > 0 {
				self.decoded_buffer.drain(..self.decoded_pos);
				self.decoded_pos = 0;
			}
		} else {
			self.decoded_buffer.clear();
			self.decoded_pos = 0;
		}

		while self.decoded_buffer.len() < len {
			trace!(self.logger, "get_next_data";
				"decoded_buffer" => self.decoded_buffer.len(),
				"decoded_pos" => self.decoded_pos,
				"len" => len,
			);

			// Decode a packet
			if let Some(packet) = self.packet_buffer.pop_front() {
				if packet.packet.len() <= 1 {
					// End of stream
					return Ok((&self.decoded_buffer, true));
				}

				self.packet_buffer_samples -= packet.samples;
				let cur_id = self.next_id;
				self.next_id = self.next_id.wrapping_add(1);
				if packet.id != cur_id {
					debug_assert!(
						packet.id.wrapping_sub(cur_id) < MAX_BUFFER_PACKETS as u16,
						"Invalid packet queue state: {} < {}",
						packet.id,
						cur_id
					);
					// Packet loss
					info!(self.logger, "Audio packet loss"; "need" => cur_id, "have" => packet.id);
					if packet.id == self.next_id {
						// Can use forward-error-correction
						self.decode_packet(Some(&packet), true)?;
					} else {
						self.decode_packet(None, false)?;
					}
					self.packet_buffer_samples += packet.samples;
					self.packet_buffer.push_front(packet);
				} else {
					self.decode_packet(Some(&packet), false)?;
				}
			} else {
				debug!(self.logger, "No packets in queue");
				// Packet loss or end of stream
				self.decode_packet(None, false)?;
			}

			if self.last_packet_samples == 0 {
				break;
			}

			// Check if we should speed-up playback
			let min = self.last_buffer_size_min.get_min();
			let dev = self.get_deviation();
			if min > (MAX_BUFFER_SIZE / USUAL_FRAME_SIZE) as u8 {
				debug!(self.logger, "Truncating buffer"; "min" => min);
				// Throw out all but min samples
				let mut keep_samples = 0;
				let keep = self
					.packet_buffer
					.iter()
					.rev()
					.take_while(|p| {
						keep_samples += p.samples;
						keep_samples < usize::from(min) + USUAL_FRAME_SIZE
					})
					.count();
				let len = self.packet_buffer.len() - keep;
				self.packet_buffer.drain(..len);
				self.packet_buffer_samples = self.packet_buffer.iter().map(|p| p.samples).sum();
				if let Some(p) = self.packet_buffer.front() {
					self.next_id = p.id;
				}
			} else if min > dev {
				// Speed-up
				debug!(self.logger, "Speed-up buffer"; "min" => min,
					"cur_packet_count" => self.packet_buffer.len(),
					"last_packet_samples" => self.last_packet_samples,
					"dev" => dev);
				let start = self.decoded_buffer.len() - self.last_packet_samples * CHANNEL_NUM;
				for i in 0..(self.last_packet_samples / SPEED_CHANGE_STEPS) {
					let i = start + i * (SPEED_CHANGE_STEPS - 1) * CHANNEL_NUM;
					self.decoded_buffer.drain(i..(i + CHANNEL_NUM));
				}
			}
		}

		self.decoded_pos = len;
		Ok((&self.decoded_buffer[..len], false))
	}
}

impl<Id: Clone + Debug + Eq + Hash + PartialEq> AudioHandler<Id> {
	pub fn new(logger: Logger) -> Self {
		Self { logger, queues: Default::default(), avg_buffer_samples: 0 }
	}

	/// Delete all queues
	pub fn reset(&mut self) { self.queues.clear(); }

	/// `buf` is not cleared before filling it.
	///
	/// Returns the clients that are not talking anymore.
	pub fn fill_buffer(&mut self, buf: &mut [f32]) -> Vec<Id> {
		self.fill_buffer_with_proc(buf, |_, _| {})
	}

	/// `buf` is not cleared before filling it.
	///
	/// Same as [`fill_buffer`] but before merging a queue into the output buffer, a preprocessor
	/// function is called. The queue volume is applied after calling the preprocessor.
	///
	/// Returns the clients that are not talking anymore.
	pub fn fill_buffer_with_proc<F: FnMut(&Id, &[f32])>(
		&mut self, buf: &mut [f32], mut handle: F,
	) -> Vec<Id> {
		trace!(self.logger, "Filling audio buffer"; "len" => buf.len());
		let mut to_remove = Vec::new();
		for (id, queue) in self.queues.iter_mut() {
			if queue.packet_loss_num >= MAX_PACKET_LOSSES {
				debug!(self.logger, "Removing talker"; "packet_loss_num" => queue.packet_loss_num);
				to_remove.push(id.clone());
				continue;
			}

			let vol = queue.volume;
			match queue.get_next_data(buf.len()) {
				Err(e) => {
					warn!(self.logger, "Failed to decode audio packet"; "error" => %e);
				}
				Ok((r, is_end)) => {
					handle(id, &r);
					for i in 0..r.len() {
						buf[i] += r[i] * vol;
					}
					if is_end {
						to_remove.push(id.clone());
					}
				}
			}
		}

		for id in &to_remove {
			self.queues.remove(&id);
		}
		to_remove
	}

	/// Add a packet to the audio queue.
	///
	/// If a new client started talking, returns the id of this client.
	pub fn handle_packet(&mut self, id: Id, sequence: u16, packet: Vec<u8>) -> Result<Option<Id>> {
		if let Some(queue) = self.queues.get_mut(&id) {
			queue.add_packet(sequence, packet)?;
			Ok(None)
		} else {

			trace!(self.logger, "Adding talker");
			let mut queue =
				AudioQueue::new(self.logger.new(o!("client" => format!("{:?}", id))), sequence,packet)?;
			if !self.queues.is_empty() {
				// Update avg_buffer_samples
				self.avg_buffer_samples = USUAL_FRAME_SIZE
					+ self
						.queues
						.values()
						.map(|q| usize::from(q.last_buffer_size_min.get_min()))
						.sum::<usize>() / self.queues.len();
			}
			queue.buffering_samples = self.avg_buffer_samples;
			self.queues.insert(id.clone(), queue);
			Ok(Some(id))
		}
	}
}