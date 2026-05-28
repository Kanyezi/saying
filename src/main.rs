#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use eframe::egui;
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{
	atomic::{AtomicBool, Ordering},
	mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::sync::mpsc as std_mpsc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;

const DISCOVERY_PORT: u16 = 42_000;
const DISCOVERY_MAGIC: &str = "SAYING_DISCOVERY";
const AUDIO_PORT: u16 = 43000;

fn main() -> eframe::Result<()> {
	std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
	std::env::set_var("MESA_LOADER_DRIVER_OVERRIDE", "llvmpipe");
	std::env::remove_var("WAYLAND_DISPLAY");

	let font_path = find_first_font_file();

	let options = eframe::NativeOptions {
		renderer: eframe::Renderer::Glow,
		hardware_acceleration: eframe::HardwareAcceleration::Preferred,
		..eframe::NativeOptions::default()
	};

	eframe::run_native(
		"saying",
		options,
		Box::new(move |cc| {
			configure_fonts(&cc.egui_ctx, font_path.as_deref());
			Box::new(MyApp::new())
		}),
	)
}

fn find_first_font_file() -> Option<std::path::PathBuf> {
	let candidates = [
		std::env::current_dir().ok().map(|dir| dir.join("font")),
		std::env::current_exe()
			.ok()
			.and_then(|path| path.parent().map(|parent| parent.join("font"))),
	];

	for font_dir in candidates.into_iter().flatten() {
		let Ok(entries) = std::fs::read_dir(&font_dir) else {
			continue;
		};

		let mut font_files: Vec<std::path::PathBuf> = entries
			.filter_map(|entry| entry.ok().map(|entry| entry.path()))
			.filter(|path| {
				path.extension()
					.and_then(|ext| ext.to_str())
					.map(|ext| ext.eq_ignore_ascii_case("ttf"))
					.unwrap_or(false)
			})
			.collect();
		font_files.sort();
		if let Some(first) = font_files.into_iter().next() {
			return Some(first);
		}
	}

	None
}

fn configure_fonts(ctx: &egui::Context, font_path: Option<&std::path::Path>) {
	let Some(font_path) = font_path else {
		return;
	};

	let Ok(font_bytes) = std::fs::read(font_path) else {
		return;
	};

	let mut fonts = egui::FontDefinitions::default();
	fonts
		.font_data
		.insert("font-file".to_string(), egui::FontData::from_owned(font_bytes));

	if let Some(fallbacks) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
		fallbacks.insert(0, "font-file".to_string());
	}

	if let Some(fallbacks) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
		fallbacks.push("font-file".to_string());
	}

	ctx.set_fonts(fonts);
}

struct MyApp {
	local_name: String,
	local_id: String,
	discovery: DiscoveryService,
	peers: HashMap<String, PeerInfo>,
	status: String,
	last_cleanup: Instant,
	input_devices: Vec<String>,
	output_devices: Vec<String>,
	selected_input: usize,
	selected_output: usize,
	audio_service: Option<AudioService>,
}

impl MyApp {
	fn new() -> Self {
		let local_name = build_local_name();
		let local_id = build_local_id(&local_name);
		let discovery = DiscoveryService::start(local_id.clone(), local_name.clone());

		// enumerate audio devices
		let host = cpal::default_host();
		let input_devices: Vec<String> = host
			.input_devices()
			.ok()
			.map(|devices| {
				devices
					.filter_map(|device| device.name().ok())
					.collect::<Vec<String>>()
			})
			.unwrap_or_default();
		let output_devices: Vec<String> = host
			.output_devices()
			.ok()
			.map(|devices| {
				devices
					.filter_map(|device| device.name().ok())
					.collect::<Vec<String>>()
			})
			.unwrap_or_default();

		Self {
			local_name,
			local_id,
			discovery,
			peers: HashMap::new(),
			status: "正在广播并扫描同一局域网的用户...".to_string(),
			last_cleanup: Instant::now(),
			input_devices,
			output_devices,
			selected_input: 0,
			selected_output: 0,
			audio_service: None,
		}
	}

	fn ingest_network_events(&mut self) {
		while let Ok(event) = self.discovery.rx.try_recv() {
			match event {
				NetEvent::PeerSeen { id, name, addr } => {
					self.peers.insert(
						id.clone(),
						PeerInfo {
							id,
							name,
							addr,
							last_seen: Instant::now(),
						},
					);
				}
				NetEvent::Status(message) => {
					self.status = message;
				}
			}
		}

		if self.last_cleanup.elapsed() >= Duration::from_secs(1) {
			self.peers
				.retain(|_, peer| peer.last_seen.elapsed() <= Duration::from_secs(6));
			self.last_cleanup = Instant::now();
		}
	}
}

impl eframe::App for MyApp {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		self.ingest_network_events();

		egui::TopBottomPanel::top("header").show(ctx, |ui| {
			ui.vertical(|ui| {
				ui.heading("局域网语音聊天 - 用户发现");
				ui.label(format!("本机身份: {}", self.local_name));
				ui.label(format!("发现通道: UDP 广播端口 {}", DISCOVERY_PORT));
				ui.label(&self.status);
			});
		});

		egui::CentralPanel::default().show(ctx, |ui| {
			ui.group(|ui| {
				ui.horizontal(|ui| {
					ui.label("输入设备:");
					egui::ComboBox::from_id_source("input_devices")
						.selected_text(
							self.input_devices
								.get(self.selected_input)
								.cloned()
								.unwrap_or_else(|| "(none)".to_string()),
						)
						.show_ui(ui, |ui| {
							for (i, name) in self.input_devices.iter().enumerate() {
								ui.selectable_value(&mut self.selected_input, i, name);
							}
						});

					ui.label("输出设备:");
					egui::ComboBox::from_id_source("output_devices")
						.selected_text(
							self.output_devices
								.get(self.selected_output)
								.cloned()
								.unwrap_or_else(|| "(none)".to_string()),
						)
						.show_ui(ui, |ui| {
							for (i, name) in self.output_devices.iter().enumerate() {
								ui.selectable_value(&mut self.selected_output, i, name);
							}
						});

					if ui.button("开始广播音频").clicked() {
						if self.audio_service.is_none() {
							let input_name = self
								.input_devices
								.get(self.selected_input)
								.cloned();
							let output_name = self
								.output_devices
								.get(self.selected_output)
								.cloned();
							let svc = AudioService::start(input_name, output_name);
							self.status = svc.status_message.clone();
							self.audio_service = Some(svc);
						}
					}

					if ui.button("停止音频").clicked() {
						if let Some(svc) = self.audio_service.take() {
							svc.stop();
							self.status = "音频已停止".to_string();
						}
					}
				});
			});

			ui.horizontal(|ui| {
				ui.label(format!("本机 ID: {}", self.local_id));
				ui.separator();
				ui.label(format!("已发现 {} 个在线用户", self.peers.len()));
			});

			ui.add_space(8.0);

			if self.peers.is_empty() {
				ui.group(|ui| {
					ui.label("还没有发现同一局域网中的其他用户。");
					ui.label("请在另一台机器启动同样的程序，或者等待几秒钟。");
				});
			} else {
				let mut peers: Vec<&PeerInfo> = self.peers.values().collect();
				peers.sort_by_key(|peer| peer.last_seen.elapsed());

				egui::ScrollArea::vertical().show(ui, |ui| {
					for peer in peers {
						let age = peer.last_seen.elapsed();
						let indicator = if age <= Duration::from_secs(2) {
							"在线"
						} else {
							"刚发现"
						};

						ui.group(|ui| {
							ui.horizontal(|ui| {
								ui.colored_label(egui::Color32::from_rgb(46, 204, 113), "●");
								ui.vertical(|ui| {
									ui.label(&peer.name);
									ui.small(format!("用户ID: {}", peer.id));
									ui.small(format!("{}  |  {}  |  {} 前", peer.addr, indicator, format_age(age)));
								});
							});
						});
						ui.add_space(6.0);
					}
				});
			}
		});

		ctx.request_repaint_after(Duration::from_millis(200));
	}
}

struct AudioService {
	stop_flag: Arc<AtomicBool>,
	input_stream: Option<cpal::Stream>,
	status_message: String,
}

impl AudioService {
	fn start(input_name: Option<String>, output_name: Option<String>) -> Self {
		let stop_flag = Arc::new(AtomicBool::new(true));
		let (snd_tx, snd_rx) = std_mpsc::sync_channel::<Vec<u8>>(64);
		let thread_stop = Arc::clone(&stop_flag);

		thread::spawn(move || {
			let socket = match std::net::UdpSocket::bind(("0.0.0.0", 0)) {
				Ok(socket) => socket,
				Err(error) => {
					eprintln!("audio udp bind failed: {error}");
					return;
				}
			};

			let _ = socket.set_broadcast(true);
			let target = ("255.255.255.255", AUDIO_PORT);

			while thread_stop.load(Ordering::Relaxed) {
				match snd_rx.recv_timeout(Duration::from_millis(200)) {
					Ok(chunk) => {
						let _ = socket.send_to(&chunk, target);
					}
					Err(std_mpsc::RecvTimeoutError::Timeout) => {}
					Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
				}
			}
		});

		let host = cpal::default_host();
		let input_stream = resolve_input_device(&host, input_name.as_deref())
			.and_then(|device| build_input_stream(&device, snd_tx));

		let _ = output_name;

		AudioService {
			stop_flag,
			input_stream,
			status_message: "音频服务已启动，正在广播输入设备采集到的声音".to_string(),
		}
	}

	fn stop(self) {
		self.stop_flag.store(false, Ordering::Relaxed);
		let _ = self.input_stream;
	}
}

fn resolve_input_device(host: &cpal::Host, input_name: Option<&str>) -> Option<cpal::Device> {
	if let Some(target_name) = input_name {
		if let Ok(devices) = host.input_devices() {
			if let Some(device) = devices
				.into_iter()
				.find(|device| device.name().ok().as_deref() == Some(target_name))
			{
				return Some(device);
			}
		}
	}

	host.default_input_device().or_else(|| {
		host.input_devices().ok()?.into_iter().next()
	})
}

fn build_input_stream(device: &cpal::Device, sender: std_mpsc::SyncSender<Vec<u8>>) -> Option<cpal::Stream> {
	let config = device.default_input_config().ok()?;
	let channels = config.channels() as usize;
	let err_fn = |error| eprintln!("input stream error: {error}");

	let stream = match config.sample_format() {
		SampleFormat::F32 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[f32], _| {
				let mut buf = Vec::with_capacity(data.len() * 2);
				for frame in data.chunks(channels) {
					let sample = frame[0].clamp(-1.0, 1.0);
					let value = (sample * i16::MAX as f32) as i16;
					buf.extend_from_slice(&value.to_le_bytes());
				}
				let _ = sender.try_send(buf);
			},
			err_fn,
			None,
		),
		SampleFormat::I16 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[i16], _| {
				let mut buf = Vec::with_capacity(data.len() * 2);
				for frame in data.chunks(channels) {
					buf.extend_from_slice(&frame[0].to_le_bytes());
				}
				let _ = sender.try_send(buf);
			},
			err_fn,
			None,
		),
		SampleFormat::U16 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[u16], _| {
				let mut buf = Vec::with_capacity(data.len() * 2);
				for frame in data.chunks(channels) {
					let value = (frame[0] as i32 - 32768) as i16;
					buf.extend_from_slice(&value.to_le_bytes());
				}
				let _ = sender.try_send(buf);
			},
			err_fn,
			None,
		),
		_ => return None,
	};

	let stream = stream.ok()?;
	if stream.play().is_err() {
		return None;
	}
	Some(stream)
}

impl Drop for MyApp {
	fn drop(&mut self) {
		self.discovery.stop();
	}
}

#[derive(Debug, Clone)]
struct PeerInfo {
	id: String,
	name: String,
	addr: SocketAddr,
	last_seen: Instant,
}

#[derive(Debug)]
enum NetEvent {
	PeerSeen { id: String, name: String, addr: SocketAddr },
	Status(String),
}

struct DiscoveryService {
	rx: mpsc::Receiver<NetEvent>,
	stop_flag: Arc<AtomicBool>,
}

impl DiscoveryService {
	fn start(local_id: String, local_name: String) -> Self {
		let (tx, rx) = mpsc::channel();
		let stop_flag = Arc::new(AtomicBool::new(true));
		let thread_stop = Arc::clone(&stop_flag);

		thread::spawn(move || {
			let socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
				Ok(socket) => socket,
				Err(error) => {
					let _ = tx.send(NetEvent::Status(format!("UDP 绑定失败: {}", error)));
					return;
				}
			};

			let _ = socket.set_broadcast(true);
			let _ = socket.set_read_timeout(Some(Duration::from_millis(250)));

			let announce = build_discovery_packet(&local_id, &local_name);
			let broadcast_target = ("255.255.255.255", DISCOVERY_PORT);
			let mut last_announce = Instant::now() - Duration::from_secs(2);
			let mut buffer = [0_u8; 1024];

			let _ = tx.send(NetEvent::Status(format!("已启动发现服务，身份: {}", local_name)));

			while thread_stop.load(Ordering::Relaxed) {
				if last_announce.elapsed() >= Duration::from_secs(1) {
					let _ = socket.send_to(announce.as_bytes(), broadcast_target);
					last_announce = Instant::now();
				}

				match socket.recv_from(&mut buffer) {
					Ok((length, addr)) => {
						if let Some(packet) = parse_discovery_packet(&buffer[..length]) {
							if packet.id != local_id {
								let _ = tx.send(NetEvent::PeerSeen {
									id: packet.id,
									name: packet.name,
									addr,
								});
							}
						}
					}
					Err(error)
						if error.kind() == std::io::ErrorKind::WouldBlock
							|| error.kind() == std::io::ErrorKind::TimedOut => {}
					Err(error) => {
						let _ = tx.send(NetEvent::Status(format!("UDP 接收异常: {}", error)));
						thread::sleep(Duration::from_millis(500));
					}
				}
			}
		});

		Self { rx, stop_flag }
	}

	fn stop(&self) {
		self.stop_flag.store(false, Ordering::Relaxed);
	}
}

fn build_local_name() -> String {
	let user = std::env::var("USER")
		.or_else(|_| std::env::var("USERNAME"))
		.unwrap_or_else(|_| "user".to_string());
	let host = std::env::var("HOSTNAME").unwrap_or_default();

	if host.is_empty() || host == user {
		user
	} else {
		format!("{}@{}", user, host)
	}
}

fn build_local_id(local_name: &str) -> String {
	let process_id = std::process::id();
	let timestamp = SystemTime::now()
		.duration_since(UNIX_EPOCH)
		.map(|duration| duration.as_nanos())
		.unwrap_or_default();
	format!("{}-{}-{}", sanitize_component(local_name), process_id, timestamp)
}

fn build_discovery_packet(local_id: &str, local_name: &str) -> String {
	format!("{}|1|{}|{}", DISCOVERY_MAGIC, local_id, sanitize_component(local_name))
}

fn parse_discovery_packet(payload: &[u8]) -> Option<DiscoveryPacket> {
	let text = std::str::from_utf8(payload).ok()?;
	let mut parts = text.splitn(4, '|');
	if parts.next()? != DISCOVERY_MAGIC {
		return None;
	}
	if parts.next()? != "1" {
		return None;
	}

	Some(DiscoveryPacket {
		id: parts.next()?.to_string(),
		name: parts.next()?.to_string(),
	})
}

fn sanitize_component(value: &str) -> String {
	value.replace('|', "_").replace('\n', " ").trim().to_string()
}

fn format_age(age: Duration) -> String {
	if age < Duration::from_secs(1) {
		format!("{}ms", age.as_millis())
	} else if age < Duration::from_secs(60) {
		format!("{:.1}s", age.as_secs_f32())
	} else {
		format!("{}m", age.as_secs() / 60)
	}
}

struct DiscoveryPacket {
	id: String,
	name: String,
}
