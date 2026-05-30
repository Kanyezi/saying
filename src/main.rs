#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use eframe::egui;
use std::collections::{HashMap, VecDeque, HashSet};
use std::net::UdpSocket;
use std::sync::mpsc as std_mpsc;
use std::sync::{
	atomic::{AtomicBool, AtomicU32, Ordering},
	mpsc, Arc, Mutex,
};
use std::thread;
use std::time::{Duration, Instant};

const DISCOVERY_PORT: u16 = 42_001;
const BAND_DISCOVERY_PORT: u16 = 715;
const DISCOVERY_MAGIC: &str = "SAYING_DISCOVERY";
const BAND_DISCOVERY_MAGIC: &str = "SAYING_BAND_DISCOVERY";
const AUDIO_MAGIC: &str = "SAYING_AUDIO";
const DEFAULT_BAND_PORT: u16 = 42_000;

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

#[derive(Default)]
struct DevicePreferences {
	user_name: Option<String>,
	input_device: Option<String>,
	output_device: Option<String>,
}

fn device_preferences_path() -> std::path::PathBuf {
	std::env::current_exe()
		.ok()
		.and_then(|path| path.parent().map(|parent| parent.join("device_prefs.txt")))
		.unwrap_or_else(|| std::path::PathBuf::from("device_prefs.txt"))
}

fn load_device_preferences() -> DevicePreferences {
	let path = device_preferences_path();
	let Ok(content) = std::fs::read_to_string(path) else {
		return DevicePreferences::default();
	};

	let mut preferences = DevicePreferences::default();
	for line in content.lines() {
		if let Some(value) = line.strip_prefix("name=") {
			if !value.trim().is_empty() {
				preferences.user_name = Some(value.trim().to_string());
			}
		} else if let Some(value) = line.strip_prefix("input=") {
			if !value.trim().is_empty() {
				preferences.input_device = Some(value.trim().to_string());
			}
		} else if let Some(value) = line.strip_prefix("output=") {
			if !value.trim().is_empty() {
				preferences.output_device = Some(value.trim().to_string());
			}
		}
	}
	preferences
}

fn save_device_preferences(
	user_name: Option<String>,
	input_device: Option<String>,
	output_device: Option<String>,
) -> std::io::Result<()> {
	let mut content = String::new();
	if let Some(name) = user_name {
		content.push_str(&format!("name={}\n", name));
	}
	if let Some(name) = input_device {
		content.push_str(&format!("input={}\n", name));
	}
	if let Some(name) = output_device {
		content.push_str(&format!("output={}\n", name));
	}
	std::fs::write(device_preferences_path(), content)
}

fn default_input_device_name(host: &cpal::Host) -> Option<String> {
	host.default_input_device().and_then(|device| device.name().ok())
}

fn default_output_device_name(host: &cpal::Host) -> Option<String> {
	host.default_output_device().and_then(|device| device.name().ok())
}

fn choose_device_index(
	devices: &[String],
	system_default: Option<String>,
	preferred: Option<&str>,
) -> usize {
	if let Some(name) = preferred.or(system_default.as_deref()) {
		if let Some(index) = devices.iter().position(|device_name| device_name == name) {
			return index;
		}
	}

	if let Some(name) = system_default.as_deref() {
		if let Some(index) = devices.iter().position(|device_name| device_name == name) {
			return index;
		}
	}

	0
}

struct MyApp {
	local_name: String,
	local_ip: String,
	discovery: DiscoveryService,
	peers: HashMap<String, PeerInfo>,
	status: String,
	last_cleanup: Instant,
	input_devices: Vec<String>,
	output_devices: Vec<String>,
	selected_input: usize,
	selected_output: usize,
	joined_bands: Vec<u16>,
	available_bands: HashMap<u16, Instant>,
	band_mic_enabled: Arc<Mutex<HashMap<u16, bool>>>,
	available_collapsed: HashSet<u16>,
	new_band_text: String,
	audio_service: Option<AudioService>,
	mic_level: f32,
	speaker_level: f32,
}

impl MyApp {
	fn new() -> Self {
		let fallback_name = build_local_name();
		let local_ip = resolve_local_ip().unwrap_or_else(|| "0.0.0.0".to_string());
		let preferences = load_device_preferences();
		let local_name = preferences.user_name.unwrap_or(fallback_name);
		let joined_bands = vec![DEFAULT_BAND_PORT];
		let discovery = DiscoveryService::start(local_ip.clone(), local_name.clone(), joined_bands.clone());
		let mut band_mic_defaults = HashMap::new();
		band_mic_defaults.insert(DEFAULT_BAND_PORT, true);
		let available_collapsed = HashSet::new();

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

		let selected_input = choose_device_index(
			&input_devices,
			default_input_device_name(&host),
			preferences.input_device.as_deref(),
		);
		let selected_output = choose_device_index(
			&output_devices,
			default_output_device_name(&host),
			preferences.output_device.as_deref(),
		);

		Self {
			local_name,
			local_ip,
			discovery,
			peers: HashMap::new(),
			status: "正在广播并扫描同一局域网的用户...".to_string(),
			last_cleanup: Instant::now(),
			input_devices,
			output_devices,
			selected_input,
			selected_output,
			joined_bands,
			available_bands: HashMap::new(),
			band_mic_enabled: Arc::new(Mutex::new(band_mic_defaults)),
			available_collapsed,
			new_band_text: String::new(),
			audio_service: None,
			mic_level: 0.0,
			speaker_level: 0.0,
		}
	}

	fn ingest_network_events(&mut self) {
		while let Ok(event) = self.discovery.rx.try_recv() {
			match event {
					NetEvent::PeerSeen { ip, name } => {
						if let Some(existing) = self.peers.get_mut(&ip) {
							existing.name = name;
							existing.last_seen = Instant::now();
						} else {
							self.peers.insert(
								ip.clone(),
								PeerInfo {
									ip,
									name,
									bands: Vec::new(),
									last_seen: Instant::now(),
								},
							);
						}
				}
				NetEvent::BandSeen { ip, name, bands } => {
					self.peers.insert(
						ip.clone(),
						PeerInfo {
							ip: ip.clone(),
							name,
							bands: bands.clone(),
							last_seen: Instant::now(),
						},
					);

					for band in bands {
						if !self.joined_bands.contains(&band) {
							let is_new_band = self.available_bands.insert(band, Instant::now()).is_none();
							if is_new_band {
								self.available_collapsed.insert(band);
							}
						}
					}
				}
				NetEvent::Status(message) => {
					self.status = message;
				}
			}
		}

		if self.last_cleanup.elapsed() >= Duration::from_secs(1) {
			self.peers
				.retain(|_, peer| peer.last_seen.elapsed() <= Duration::from_secs(6));
			self.available_bands
				.retain(|_, last_seen| last_seen.elapsed() <= Duration::from_secs(6));
			self.available_collapsed.retain(|band| self.available_bands.contains_key(band));
			self.last_cleanup = Instant::now();
		}

		if let Some(service) = self.audio_service.as_ref() {
			self.mic_level = service.mic_level();
			self.speaker_level = service.speaker_level();
		} else {
			self.mic_level = 0.0;
			self.speaker_level = 0.0;
		}
	}

	fn join_band(&mut self, band: u16) {
		if self.joined_bands.contains(&band) {
			return;
		}

		self.available_bands.remove(&band);
		self.joined_bands.push(band);
		self.joined_bands.sort_unstable();
		if let Ok(mut mic_states) = self.band_mic_enabled.lock() {
			mic_states.insert(band, true);
		}
		if let Some(service) = self.audio_service.as_ref() {
			service.set_bands(self.joined_bands.clone());
		}
		self.discovery.set_local_bands(self.joined_bands.clone());
		self.status = format!("已加入频段 {}", band);
	}

	fn leave_band(&mut self, band: u16) {
		if !self.joined_bands.contains(&band) {
			return;
		}

		self.joined_bands.retain(|value| *value != band);
		if let Ok(mut mic_states) = self.band_mic_enabled.lock() {
			mic_states.remove(&band);
		}
		if let Some(service) = self.audio_service.as_ref() {
			service.set_bands(self.joined_bands.clone());
		}
		self.discovery.set_local_bands(self.joined_bands.clone());
		self.status = format!("已退出频段 {}", band);
	}

	fn toggle_band_mic(&mut self, band: u16) {
		if !self.joined_bands.contains(&band) {
			return;
		}

		if let Ok(mut mic_states) = self.band_mic_enabled.lock() {
			let next_state = !mic_states.get(&band).copied().unwrap_or(true);
			mic_states.insert(band, next_state);
			self.status = if next_state {
				format!("频段 {} 麦克风已启用", band)
			} else {
				format!("频段 {} 麦克风已禁用", band)
			};
		}
	}

	fn is_band_mic_enabled(&self, band: u16) -> bool {
		self.band_mic_enabled
			.lock()
			.ok()
			.and_then(|states| states.get(&band).copied())
			.unwrap_or(true)
	}
}

impl eframe::App for MyApp {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		self.ingest_network_events();

		egui::TopBottomPanel::top("header").show(ctx, |ui| {
			ui.vertical(|ui| {
				ui.heading("局域网语音聊天 - 用户发现");
				ui.horizontal(|ui| {
					ui.label("本机名称:");
					let response = ui.text_edit_singleline(&mut self.local_name);
					if response.changed() {
						self.discovery.set_local_name(self.local_name.clone());
						let _ = save_device_preferences(
							Some(self.local_name.clone()),
							self.input_devices.get(self.selected_input).cloned(),
							self.output_devices.get(self.selected_output).cloned(),
						);
					}
				});
				ui.label(format!("本机 IP: {}", self.local_ip));
				ui.label(&self.status);
			});
		});

		egui::CentralPanel::default().show(ctx, |ui| {
			ui.group(|ui| {
				let mut device_selection_changed = false;

				ui.horizontal(|ui| {
					ui.label("添加频段端口:");
					ui.text_edit_singleline(&mut self.new_band_text);
					if ui.button("创建新的频段").clicked() {
						if let Ok(port) = self.new_band_text.trim().parse::<u16>() {
							if !self.joined_bands.contains(&port) {
								self.join_band(port);
								self.status = format!("已创建并加入频段 {}", port);
							}
						}
						self.new_band_text.clear();
					}
				});

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
								device_selection_changed |= ui
									.selectable_value(&mut self.selected_input, i, name)
									.changed();
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
								device_selection_changed |= ui
									.selectable_value(&mut self.selected_output, i, name)
									.changed();
							}
						});

					if device_selection_changed {
						let _ = save_device_preferences(
							Some(self.local_name.clone()),
							self.input_devices.get(self.selected_input).cloned(),
							self.output_devices.get(self.selected_output).cloned(),
						);
					}

					if ui.button("开始广播音频").clicked() {
						if self.audio_service.is_none() {
							let input_name = self.input_devices.get(self.selected_input).cloned();
							let output_name = self.output_devices.get(self.selected_output).cloned();
							let svc = AudioService::start(
								self.local_ip.clone(),
								input_name,
								output_name,
								self.joined_bands.clone(),
								Arc::clone(&self.band_mic_enabled),
							);
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

				ui.add_space(8.0);
				ui.horizontal(|ui| {
					ui.label("麦克风音量");
					ui.add(
						egui::ProgressBar::new(self.mic_level)
							.show_percentage()
							.desired_width(180.0),
					);
					ui.label(format!("{:.0}%", self.mic_level * 100.0));
				});

				ui.horizontal(|ui| {
					ui.label("扬声器音量");
					ui.add(
						egui::ProgressBar::new(self.speaker_level)
							.show_percentage()
							.desired_width(180.0),
					);
					ui.label(format!("{:.0}%", self.speaker_level * 100.0));
				});
			});

			ui.horizontal(|ui| {
				ui.label(format!("本机 IP: {}", self.local_ip));
				ui.separator();
				ui.label(format!("已发现 {} 个在线用户", self.peers.len()));
			});

			ui.add_space(8.0);

			ui.label("加入的频段列表");
			ui.add_space(6.0);
			egui::ScrollArea::vertical().show(ui, |ui| {
				for band in self.joined_bands.clone() {
					ui.group(|ui| {
						ui.label(format!("音频频段: {}", band));
						ui.add_space(6.0);
						ui.horizontal(|ui| {
							let mic_label = if self.is_band_mic_enabled(band) {
								"禁用麦克风"
							} else {
								"启用麦克风"
							};
							if ui.button(mic_label).clicked() {
								self.toggle_band_mic(band);
							}
							if ui.button("退出").clicked() {
								self.leave_band(band);
							}
						});
						ui.horizontal_wrapped(|ui| {
							for peer in self.peers.values().filter(|peer| peer.bands.contains(&band)) {
								let (rect, _resp) = ui.allocate_exact_size(egui::vec2(200.0, 80.0), egui::Sense::hover());
								let frame = egui::Frame::none().stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200)));
								ui.allocate_ui_at_rect(rect, |ui| {
									frame.show(ui, |ui| {
										ui.vertical_centered(|ui| {
											ui.label(&peer.name);
											ui.small(format!("ip: {}", peer.ip));
											let age = peer.last_seen.elapsed();
											let indicator = if age <= Duration::from_secs(2) { "在线" } else { "刚发现" };
											ui.small(format!("{} | {} 前", indicator, format_age(age)));
										});
									});
								});
							}
						});
					});
					ui.add_space(8.0);
				}
			});

			ui.add_space(10.0);
			ui.label("识别到的频段:");
			ui.add_space(6.0);
			let mut available_bands: Vec<u16> = self.available_bands.keys().copied().collect();
			available_bands.sort_unstable();
			for band in available_bands {
				let collapsed = self.available_collapsed.contains(&band);
				ui.group(|ui| {
					ui.horizontal(|ui| {
						if collapsed {
							// collapsed: full-width clickable header
							let header = format!("音频频段: {} ▸", band);
							if ui.add(egui::Button::new(header).min_size(egui::vec2(ui.available_width(), 24.0))).clicked() {
								self.available_collapsed.remove(&band);
							}
						} else {
							ui.label(format!("音频频段: {}", band));
							if ui.button("折叠").clicked() {
								self.available_collapsed.insert(band);
							}
							if ui.button("加入").clicked() {
								self.join_band(band);
							}
						}
					});

					if !collapsed {
						ui.add_space(6.0);
						ui.horizontal_wrapped(|ui| {
							for peer in self.peers.values().filter(|p| p.bands.contains(&band)) {
								let (rect, _resp) = ui.allocate_exact_size(egui::vec2(200.0, 80.0), egui::Sense::hover());
								let frame = egui::Frame::none().stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200)));
								ui.allocate_ui_at_rect(rect, |ui| {
									frame.show(ui, |ui| {
										ui.vertical_centered(|ui| {
											ui.label(&peer.name);
											ui.small(format!("ip: {}", peer.ip));
											let age = peer.last_seen.elapsed();
											let indicator = if age <= Duration::from_secs(2) { "在线" } else { "刚发现" };
											ui.small(format!("{} | {} 前", indicator, format_age(age)));
										});
									});
								});
							}
						});
					}
				});
				ui.add_space(8.0);
			}

			if self.available_bands.is_empty() {
				ui.label("当前没有发现新的频段。");
			}
		});

		ctx.request_repaint_after(Duration::from_millis(200));
	}
}

impl Drop for MyApp {
	fn drop(&mut self) {
		self.discovery.stop();
	}
}

struct AudioService {
	stop_flag: Arc<AtomicBool>,
	input_stream: Option<cpal::Stream>,
	output_stream: Option<cpal::Stream>,
	metrics: Arc<AudioMetrics>,
	joined_bands: Arc<Mutex<Vec<u16>>>,
	status_message: String,
}

impl AudioService {
	fn start(
		local_ip: String,
		input_name: Option<String>,
		output_name: Option<String>,
		bands: Vec<u16>,
		band_mic_enabled: Arc<Mutex<HashMap<u16, bool>>>,
	) -> Self {
		let stop_flag = Arc::new(AtomicBool::new(true));
		let metrics = Arc::new(AudioMetrics::default());
		let playback_queue = Arc::new(Mutex::new(VecDeque::<i16>::new()));
		let (snd_tx, snd_rx) = std_mpsc::sync_channel::<Vec<u8>>(64);
		let joined_bands = Arc::new(Mutex::new(bands));
		let bands_for_send = Arc::clone(&joined_bands);
		let band_mic_enabled_for_send = Arc::clone(&band_mic_enabled);

		let sender_stop = Arc::clone(&stop_flag);
		thread::spawn(move || {
			let socket = match UdpSocket::bind(("0.0.0.0", 0)) {
				Ok(socket) => socket,
				Err(error) => {
					eprintln!("audio udp bind failed: {error}");
					return;
				}
			};

			let _ = socket.set_broadcast(true);

			while sender_stop.load(Ordering::Relaxed) {
				match snd_rx.recv_timeout(Duration::from_millis(200)) {
					Ok(packet) => {
						let current_bands = bands_for_send
							.lock()
							.map(|bands| bands.clone())
							.unwrap_or_default();
						for band in &current_bands {
							let enabled = band_mic_enabled_for_send
								.lock()
								.ok()
								.and_then(|states| states.get(band).copied())
								.unwrap_or(false);
							if enabled {
								let target = ("255.255.255.255", *band);
								let _ = socket.send_to(&packet, target);
							}
						}
					}
					Err(std_mpsc::RecvTimeoutError::Timeout) => {}
					Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
				}
			}
		});

		let host = cpal::default_host();
		let output_rate = resolve_output_device(&host, output_name.as_deref())
			.and_then(|device| device.default_output_config().ok().map(|config| config.sample_rate().0));

		let input_stream = resolve_input_device(&host, input_name.as_deref()).and_then(|device| {
			build_input_stream(
				&device,
				snd_tx,
				Arc::clone(&metrics),
				local_ip.clone(),
			)
		});

		let output_stream = resolve_output_device(&host, output_name.as_deref())
			.and_then(|device| build_output_stream(&device, Arc::clone(&playback_queue)));

		let local_ip_for_recv = local_ip.clone();
		let receiver_stop = Arc::clone(&stop_flag);
		let receiver_queue = Arc::clone(&playback_queue);
		let receiver_metrics = Arc::clone(&metrics);
		let bands_for_recv = Arc::clone(&joined_bands);
		thread::spawn(move || {
			let mut buffer = [0_u8; 4096];
			let mut current_bands: Vec<u16> = Vec::new();
			let mut sockets: Vec<UdpSocket> = Vec::new();

			while receiver_stop.load(Ordering::Relaxed) {
				let desired_bands = bands_for_recv
					.lock()
					.map(|bands| bands.clone())
					.unwrap_or_default();
				rebuild_receiver_sockets(&desired_bands, &mut current_bands, &mut sockets);

				if sockets.is_empty() {
					thread::sleep(Duration::from_millis(200));
					continue;
				}

				for socket in &sockets {
					match socket.recv_from(&mut buffer) {
						Ok((length, addr)) => {
							if addr.ip().to_string() == local_ip_for_recv {
								continue;
							}
							if let Some((source_ip, sample_rate, samples)) = parse_audio_packet(&buffer[..length]) {
								if source_ip == local_ip_for_recv {
									continue;
								}

								receiver_metrics.set_speaker_level(audio_level_i16(&samples));
								let playback_samples = match output_rate {
									Some(target_rate) if target_rate != sample_rate => {
										resample_i16_mono(&samples, sample_rate, target_rate)
									}
									_ => samples,
								};

								if let Ok(mut queue) = receiver_queue.lock() {
									queue.extend(playback_samples);
								}
							}
						}
						Err(error)
							if error.kind() == std::io::ErrorKind::WouldBlock
								|| error.kind() == std::io::ErrorKind::TimedOut => {}
						Err(error) => {
							eprintln!("audio recv error: {error}");
						}
					}
				}
			}
		});

		Self {
			stop_flag,
			input_stream,
			output_stream,
			metrics,
			joined_bands,
			status_message: "音频服务已启动，正在广播输入设备采集到的声音".to_string(),
		}
	}

	fn set_bands(&self, bands: Vec<u16>) {
		if let Ok(mut current) = self.joined_bands.lock() {
			*current = bands;
		}
	}

	fn stop(self) {
		self.stop_flag.store(false, Ordering::Relaxed);
		let _ = self.input_stream;
		let _ = self.output_stream;
	}

	fn mic_level(&self) -> f32 {
		self.metrics.mic_level()
	}

	fn speaker_level(&self) -> f32 {
		self.metrics.speaker_level()
	}
}

#[derive(Default)]
struct AudioMetrics {
	mic_level_bits: AtomicU32,
	speaker_level_bits: AtomicU32,
}

impl AudioMetrics {
	fn mic_level(&self) -> f32 {
		f32::from_bits(self.mic_level_bits.load(Ordering::Relaxed))
	}

	fn speaker_level(&self) -> f32 {
		f32::from_bits(self.speaker_level_bits.load(Ordering::Relaxed))
	}

	fn set_mic_level(&self, level: f32) {
		self.mic_level_bits
			.store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
	}

	fn set_speaker_level(&self, level: f32) {
		self.speaker_level_bits
			.store(level.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
	}
}

fn resolve_local_ip() -> Option<String> {
	let socket = UdpSocket::bind(("0.0.0.0", 0)).ok()?;
	let _ = socket.connect(("8.8.8.8", 80));
	socket.local_addr().ok().map(|addr| addr.ip().to_string())
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

	host.default_input_device().or_else(|| host.input_devices().ok()?.into_iter().next())
}

fn resolve_output_device(host: &cpal::Host, output_name: Option<&str>) -> Option<cpal::Device> {
	if let Some(target_name) = output_name {
		if let Ok(devices) = host.output_devices() {
			if let Some(device) = devices
				.into_iter()
				.find(|device| device.name().ok().as_deref() == Some(target_name))
			{
				return Some(device);
			}
		}
	}

	host.default_output_device().or_else(|| host.output_devices().ok()?.into_iter().next())
}

fn build_input_stream(
	device: &cpal::Device,
	sender: std_mpsc::SyncSender<Vec<u8>>,
	metrics: Arc<AudioMetrics>,
	local_id: String,
) -> Option<cpal::Stream> {
	let config = device.default_input_config().ok()?;
	let sample_rate = config.sample_rate().0;
	let channels = config.channels() as usize;
	let err_fn = |error| eprintln!("input stream error: {error}");

	let stream = match config.sample_format() {
		SampleFormat::F32 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[f32], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					let sample = frame[0].clamp(-1.0, 1.0);
					samples.push((sample * i16::MAX as f32) as i16);
				}
				metrics.set_mic_level(audio_level_i16(&samples));
				let packet = build_audio_packet(&local_id, sample_rate, &samples);
				let _ = sender.try_send(packet);
			},
			err_fn,
			None,
		),
		SampleFormat::I16 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[i16], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					samples.push(frame[0]);
				}
				metrics.set_mic_level(audio_level_i16(&samples));
				let packet = build_audio_packet(&local_id, sample_rate, &samples);
				let _ = sender.try_send(packet);
			},
			err_fn,
			None,
		),
		SampleFormat::U16 => device.build_input_stream(
			&config.clone().into(),
			move |data: &[u16], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					samples.push((frame[0] as i32 - 32768) as i16);
				}
				metrics.set_mic_level(audio_level_i16(&samples));
				let packet = build_audio_packet(&local_id, sample_rate, &samples);
				let _ = sender.try_send(packet);
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

fn build_output_stream(
	device: &cpal::Device,
	queue: Arc<Mutex<VecDeque<i16>>>,
) -> Option<cpal::Stream> {
	let config = device.default_output_config().ok()?;
	let channels = config.channels() as usize;
	let err_fn = |error| eprintln!("output stream error: {error}");

	let stream = match config.sample_format() {
		SampleFormat::F32 => device.build_output_stream(
			&config.clone().into(),
			move |output: &mut [f32], _| {
				fill_output_buffer(output, channels, &queue, |sample| sample as f32 / i16::MAX as f32);
			},
			err_fn,
			None,
		),
		SampleFormat::I16 => device.build_output_stream(
			&config.clone().into(),
			move |output: &mut [i16], _| {
				fill_output_buffer(output, channels, &queue, |sample| sample);
			},
			err_fn,
			None,
		),
		SampleFormat::U16 => device.build_output_stream(
			&config.clone().into(),
			move |output: &mut [u16], _| {
				fill_output_buffer(output, channels, &queue, |sample| (sample as i32 + 32768) as u16);
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

fn fill_output_buffer<T, F>(
	data: &mut [T],
	channels: usize,
	queue: &Arc<Mutex<VecDeque<i16>>>,
	convert: F,
)
where
	T: Copy,
	F: Fn(i16) -> T,
{
	let Ok(mut guard) = queue.lock() else {
		return;
	};

	for frame in data.chunks_mut(channels) {
		let sample = guard.pop_front().unwrap_or(0);
		let value = convert(sample);
		for item in frame.iter_mut() {
			*item = value;
		}
	}
}

fn build_audio_packet(local_id: &str, sample_rate: u32, samples: &[i16]) -> Vec<u8> {
	let mut packet = Vec::with_capacity(AUDIO_MAGIC.len() + local_id.len() + samples.len() * 2 + 32);
	packet.extend_from_slice(AUDIO_MAGIC.as_bytes());
	packet.push(b'|');
	packet.extend_from_slice(b"1|");
	packet.extend_from_slice(local_id.as_bytes());
	packet.push(b'|');
	packet.extend_from_slice(sample_rate.to_string().as_bytes());
	packet.push(b'|');
	for sample in samples {
		packet.extend_from_slice(&sample.to_le_bytes());
	}
	packet
}

fn rebuild_receiver_sockets(desired_bands: &[u16], current_bands: &mut Vec<u16>, sockets: &mut Vec<UdpSocket>) {
	if desired_bands == current_bands.as_slice() {
		return;
	}

	let mut next_sockets = Vec::new();
	for band in desired_bands {
		match UdpSocket::bind(("0.0.0.0", *band)) {
			Ok(socket) => {
				let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
				next_sockets.push(socket);
			}
			Err(error) => {
				eprintln!("audio recv bind failed on band {band}: {error}");
			}
		}
	}

	*current_bands = desired_bands.to_vec();
	*sockets = next_sockets;
}

fn parse_audio_packet(payload: &[u8]) -> Option<(String, u32, Vec<i16>)> {
	let first = payload.iter().position(|b| *b == b'|')?;
	let second = payload[first + 1..].iter().position(|b| *b == b'|')? + first + 1;
	let third = payload[second + 1..].iter().position(|b| *b == b'|')? + second + 1;
	let fourth = payload[third + 1..].iter().position(|b| *b == b'|')? + third + 1;

	if &payload[..first] != AUDIO_MAGIC.as_bytes() {
		return None;
	}
	if &payload[first + 1..second] != b"1" {
		return None;
	}

	let source_id = std::str::from_utf8(&payload[second + 1..third]).ok()?.to_string();
	let sample_rate = std::str::from_utf8(&payload[third + 1..fourth]).ok()?.parse().ok()?;
	let samples = bytes_to_i16_samples(&payload[fourth + 1..]);
	Some((source_id, sample_rate, samples))
}

fn bytes_to_i16_samples(payload: &[u8]) -> Vec<i16> {
	payload
		.chunks_exact(2)
		.map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
		.collect()
}

fn audio_level_i16(samples: &[i16]) -> f32 {
	if samples.is_empty() {
		return 0.0;
	}
	let peak = samples
		.iter()
		.map(|sample| ((*sample as i32).unsigned_abs() as f32) / i16::MAX as f32)
		.fold(0.0f32, f32::max);
	peak.clamp(0.0, 1.0)
}

fn resample_i16_mono(samples: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
	if samples.is_empty() || input_rate == 0 || output_rate == 0 || input_rate == output_rate {
		return samples.to_vec();
	}

	let ratio = output_rate as f32 / input_rate as f32;
	let output_len = ((samples.len() as f32) * ratio).max(1.0) as usize;
	let mut output = Vec::with_capacity(output_len);

	for index in 0..output_len {
		let source_pos = index as f32 / ratio;
		let left_index = source_pos.floor() as usize;
		let right_index = (left_index + 1).min(samples.len().saturating_sub(1));
		let fraction = source_pos - left_index as f32;
		let left = samples[left_index] as f32;
		let right = samples[right_index] as f32;
		let interpolated = left + (right - left) * fraction;
		output.push(interpolated.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
	}

	output
}

#[derive(Debug, Clone)]
struct PeerInfo {
	ip: String,
	name: String,
	bands: Vec<u16>,
	last_seen: Instant,
}

#[derive(Debug)]
enum NetEvent {
	PeerSeen { ip: String, name: String },
	BandSeen { ip: String, name: String, bands: Vec<u16> },
	Status(String),
}

struct DiscoveryService {
	rx: mpsc::Receiver<NetEvent>,
	stop_flag: Arc<AtomicBool>,
	local_name: Arc<Mutex<String>>,
	local_bands: Arc<Mutex<Vec<u16>>>,
}

impl DiscoveryService {
	fn start(local_ip: String, local_name: String, local_bands: Vec<u16>) -> Self {
		let (tx, rx) = mpsc::channel();
		let stop_flag = Arc::new(AtomicBool::new(true));
		let local_name = Arc::new(Mutex::new(local_name));
		let local_bands = Arc::new(Mutex::new(local_bands));
		let thread_stop = Arc::clone(&stop_flag);
		let thread_local_name = Arc::clone(&local_name);
		let thread_local_bands = Arc::clone(&local_bands);

		thread::spawn(move || {
			let name_socket = match UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT)) {
				Ok(socket) => socket,
				Err(error) => {
					let _ = tx.send(NetEvent::Status(format!("名称发现 UDP 绑定失败: {}", error)));
					return;
				}
			};

			let band_socket = match UdpSocket::bind(("0.0.0.0", BAND_DISCOVERY_PORT)) {
				Ok(socket) => socket,
				Err(error) => {
					let _ = tx.send(NetEvent::Status(format!("频段发现 UDP 绑定失败: {}", error)));
					return;
				}
			};

			let _ = name_socket.set_broadcast(true);
			let _ = name_socket.set_read_timeout(Some(Duration::from_millis(100)));
			let _ = band_socket.set_broadcast(true);
			let _ = band_socket.set_read_timeout(Some(Duration::from_millis(100)));

			let name_broadcast_target = ("255.255.255.255", DISCOVERY_PORT);
			let band_broadcast_target = ("255.255.255.255", BAND_DISCOVERY_PORT);
			let mut last_announce = Instant::now() - Duration::from_secs(2);
			let mut name_buffer = [0_u8; 1024];
			let mut band_buffer = [0_u8; 1024];

			let initial_name = thread_local_name
				.lock()
				.map(|name| name.clone())
				.unwrap_or_else(|_| "user".to_string());
			let _ = tx.send(NetEvent::Status(format!("已启动发现服务，身份: {}", initial_name)));

			while thread_stop.load(Ordering::Relaxed) {
				if last_announce.elapsed() >= Duration::from_secs(1) {
					let current_name = thread_local_name
						.lock()
						.map(|name| name.clone())
						.unwrap_or_else(|_| "user".to_string());
					let current_bands = thread_local_bands
						.lock()
						.map(|bands| bands.clone())
						.unwrap_or_default();
					let name_announce = build_discovery_packet(&local_ip, &current_name);
					let band_announce = build_band_discovery_packet(&local_ip, &current_name, &current_bands);
					let _ = name_socket.send_to(name_announce.as_bytes(), name_broadcast_target);
					let _ = band_socket.send_to(band_announce.as_bytes(), band_broadcast_target);
					last_announce = Instant::now();
				}

				match name_socket.recv_from(&mut name_buffer) {
					Ok((length, addr)) => {
							if let Some(packet) = parse_discovery_packet(&name_buffer[..length]) {
								let peer_ip = addr.ip().to_string();
								if peer_ip != local_ip {
									let _ = tx.send(NetEvent::PeerSeen {
										ip: peer_ip,
										name: packet.name,
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

				match band_socket.recv_from(&mut band_buffer) {
					Ok((length, addr)) => {
						if let Some(packet) = parse_band_discovery_packet(&band_buffer[..length]) {
							let peer_ip = addr.ip().to_string();
							if peer_ip != local_ip {
								let _ = tx.send(NetEvent::BandSeen {
									ip: peer_ip,
									name: packet.name,
									bands: packet.bands,
								});
							}
						}
					}
					Err(error)
						if error.kind() == std::io::ErrorKind::WouldBlock
							|| error.kind() == std::io::ErrorKind::TimedOut => {}
					Err(error) => {
						let _ = tx.send(NetEvent::Status(format!("频段接收异常: {}", error)));
					}
				}
			}
		});

		Self {
			rx,
			stop_flag,
			local_name,
			local_bands,
		}
	}

	fn stop(&self) {
		self.stop_flag.store(false, Ordering::Relaxed);
	}

	fn set_local_name(&self, new_name: String) {
		if let Ok(mut name) = self.local_name.lock() {
			*name = new_name;
		}
	}

	fn set_local_bands(&self, bands: Vec<u16>) {
		if let Ok(mut current) = self.local_bands.lock() {
			*current = bands;
		}
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

fn build_discovery_packet(local_ip: &str, local_name: &str) -> String {
	format!("{}|1|{}|{}", DISCOVERY_MAGIC, local_ip, sanitize_component(local_name))
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
		name: parts.next()?.to_string(),
	})
}

fn build_band_discovery_packet(local_ip: &str, local_name: &str, bands: &[u16]) -> String {
	let band_list = bands.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
	format!(
		"{}|1|{}|{}|{}",
		BAND_DISCOVERY_MAGIC,
		local_ip,
		sanitize_component(local_name),
		band_list
	)
}

fn parse_band_discovery_packet(payload: &[u8]) -> Option<BandDiscoveryPacket> {
	let text = std::str::from_utf8(payload).ok()?;
	let mut parts = text.splitn(5, '|');
	if parts.next()? != BAND_DISCOVERY_MAGIC {
		return None;
	}
	if parts.next()? != "1" {
		return None;
	}

	let _ip = parts.next()?.to_string();
	let name = parts.next()?.to_string();
	let bands = parts
		.next()?
		.split(',')
		.filter_map(|band| band.trim().parse::<u16>().ok())
		.collect::<Vec<_>>();

	Some(BandDiscoveryPacket { name, bands })
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
	name: String,
}

struct BandDiscoveryPacket {
	name: String,
	bands: Vec<u16>,
}
