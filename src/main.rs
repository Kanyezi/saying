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
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

#[cfg(target_os = "windows")]
use winreg::{enums::HKEY_CURRENT_USER, RegKey};

const DISCOVERY_PORT: u16 = 42_001;
const BAND_DISCOVERY_PORT: u16 = 715;
const DISCOVERY_MAGIC: &str = "SAYING_DISCOVERY";
const BAND_DISCOVERY_MAGIC: &str = "SAYING_BAND_DISCOVERY";
const AUDIO_MAGIC: &str = "SAYING_AUDIO";
const DEFAULT_BAND_PORT: u16 = 42_000;
const LOCAL_SPEAKING_THRESHOLD: f32 = 0.12;
const MIC_BROADCAST_START_THRESHOLD: f32 = 0.16;
const MIC_BROADCAST_STOP_THRESHOLD: f32 = 0.08;
const MIC_BROADCAST_HOLD_DURATION: Duration = Duration::from_millis(180);

#[derive(Clone, Copy)]
struct MicGateConfig {
	start_threshold: f32,
	stop_threshold: f32,
	hold_ms: u32,
}

impl Default for MicGateConfig {
	fn default() -> Self {
		Self {
			start_threshold: MIC_BROADCAST_START_THRESHOLD,
			stop_threshold: MIC_BROADCAST_STOP_THRESHOLD,
			hold_ms: MIC_BROADCAST_HOLD_DURATION.as_millis() as u32,
		}
	}
}

impl MicGateConfig {
	fn normalized(mut self) -> Self {
		self.start_threshold = self.start_threshold.clamp(0.0, 1.0);
		self.stop_threshold = self.stop_threshold.clamp(0.0, self.start_threshold);
		self.hold_ms = self.hold_ms.min(2_000);
		self
	}

	fn hold_duration(&self) -> Duration {
		Duration::from_millis(self.hold_ms as u64)
	}
}

fn main() -> eframe::Result<()> {
	let autostart_launch = std::env::args().any(|arg| arg == "--autostart");
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
			Box::new(MyApp::new(autostart_launch))
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
	mic_gate_config: MicGateConfig,
	autostart_enabled: bool,
	autostart_minimize_to_tray: bool,
	autostart_auto_broadcast: bool,
	autostart_auto_receive: bool,
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
		} else if let Some(value) = line.strip_prefix("gate_start=") {
			if let Ok(parsed) = value.trim().parse::<f32>() {
				preferences.mic_gate_config.start_threshold = parsed;
			}
		} else if let Some(value) = line.strip_prefix("gate_stop=") {
			if let Ok(parsed) = value.trim().parse::<f32>() {
				preferences.mic_gate_config.stop_threshold = parsed;
			}
		} else if let Some(value) = line.strip_prefix("gate_hold=") {
			if let Ok(parsed) = value.trim().parse::<u32>() {
				preferences.mic_gate_config.hold_ms = parsed;
			}
		} else if let Some(value) = line.strip_prefix("autostart=") {
			preferences.autostart_enabled = value.trim() == "1";
		} else if let Some(value) = line.strip_prefix("autostart_minimize_to_tray=") {
			preferences.autostart_minimize_to_tray = value.trim() == "1";
		} else if let Some(value) = line.strip_prefix("autostart_auto_broadcast=") {
			preferences.autostart_auto_broadcast = value.trim() == "1";
		} else if let Some(value) = line.strip_prefix("autostart_auto_receive=") {
			preferences.autostart_auto_receive = value.trim() == "1";
		}
	}
	preferences.mic_gate_config = preferences.mic_gate_config.normalized();
	preferences
}

fn save_device_preferences(
	user_name: Option<String>,
	input_device: Option<String>,
	output_device: Option<String>,
	mic_gate_config: MicGateConfig,
	autostart_enabled: bool,
	autostart_minimize_to_tray: bool,
	autostart_auto_broadcast: bool,
	autostart_auto_receive: bool,
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
	let mic_gate_config = mic_gate_config.normalized();
	content.push_str(&format!("gate_start={:.3}\n", mic_gate_config.start_threshold));
	content.push_str(&format!("gate_stop={:.3}\n", mic_gate_config.stop_threshold));
	content.push_str(&format!("gate_hold={}\n", mic_gate_config.hold_ms));
	content.push_str(&format!("autostart={}\n", if autostart_enabled { 1 } else { 0 }));
	content.push_str(&format!("autostart_minimize_to_tray={}\n", if autostart_minimize_to_tray { 1 } else { 0 }));
	content.push_str(&format!("autostart_auto_broadcast={}\n", if autostart_auto_broadcast { 1 } else { 0 }));
	content.push_str(&format!("autostart_auto_receive={}\n", if autostart_auto_receive { 1 } else { 0 }));
	std::fs::write(device_preferences_path(), content)
}

#[cfg(target_os = "windows")]
fn update_windows_autostart(enabled: bool) -> std::io::Result<()> {
	let hkcu = RegKey::predef(HKEY_CURRENT_USER);
	let run_key_path = r"Software\Microsoft\Windows\CurrentVersion\Run";
	if enabled {
		let (run_key, _) = hkcu.create_subkey(run_key_path)?;
		let exe = std::env::current_exe()?;
		let command = format!("\"{}\" --autostart", exe.display());
		run_key.set_value("saying", &command)?;
	} else if let Ok(run_key) = hkcu.open_subkey(run_key_path) {
		let _ = run_key.delete_value("saying");
	}
	Ok(())
}

#[cfg(not(target_os = "windows"))]
fn update_windows_autostart(_enabled: bool) -> std::io::Result<()> {
	Ok(())
}

fn make_tray_icon() -> Result<Icon, String> {
	let width = 32;
	let height = 32;
	let mut rgba = vec![0u8; width * height * 4];
	let center = 16.0f32;
	for y in 0..height {
		for x in 0..width {
			let dx = x as f32 - center;
			let dy = y as f32 - center;
			let distance = (dx * dx + dy * dy).sqrt();
			let offset = (y * width + x) * 4;
			if distance <= 11.5 {
				rgba[offset] = 56;
				rgba[offset + 1] = 185;
				rgba[offset + 2] = 129;
				rgba[offset + 3] = 255;
			} else if distance <= 14.0 {
				rgba[offset] = 230;
				rgba[offset + 1] = 236;
				rgba[offset + 2] = 240;
				rgba[offset + 3] = 200;
			}
		}
	}
	Icon::from_rgba(rgba, width as u32, height as u32).map_err(|err| err.to_string())
}

fn fallback_tray_icon() -> Icon {
	let width = 32;
	let height = 32;
	let mut rgba = vec![0u8; width * height * 4];
	for pixel in rgba.chunks_exact_mut(4) {
		pixel[0] = 60;
		pixel[1] = 179;
		pixel[2] = 113;
		pixel[3] = 255;
	}
	Icon::from_rgba(rgba, width as u32, height as u32).unwrap()
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
	tray_icon: Option<TrayIcon>,
	tray_exit_item: MenuItem,
	window_hidden: bool,
	exit_requested: bool,
	autostart_enabled: bool,
	autostart_minimize_to_tray: bool,
	autostart_auto_broadcast: bool,
	autostart_auto_receive: bool,
}

impl MyApp {
	fn new(autostart_launch: bool) -> Self {
		let fallback_name = build_local_name();
		let local_ip = resolve_local_ip().unwrap_or_else(|| "0.0.0.0".to_string());
		let preferences = load_device_preferences();
		let local_name = preferences.user_name.unwrap_or(fallback_name);
		let joined_bands = vec![DEFAULT_BAND_PORT];
		let discovery = DiscoveryService::start(local_ip.clone(), local_name.clone(), joined_bands.clone());
		let mut band_mic_defaults = HashMap::new();
		band_mic_defaults.insert(DEFAULT_BAND_PORT, true);
		let available_collapsed = HashSet::new();
		let band_mic_enabled = Arc::new(Mutex::new(band_mic_defaults));
		let mic_gate_config = Arc::new(Mutex::new(preferences.mic_gate_config));

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
		let autostart_launch = autostart_launch && preferences.autostart_enabled;
		let window_hidden = autostart_launch && preferences.autostart_minimize_to_tray;
		let tray_exit_item = MenuItem::new("退出", true, None);
		let mut audio_service = Some(AudioService::new(
			local_ip.clone(),
			preferences.output_device.clone(),
			joined_bands.clone(),
			Arc::clone(&band_mic_enabled),
			mic_gate_config,
		));
		if let Some(service) = audio_service.as_mut() {
			service.set_receive_audio_enabled(!autostart_launch || preferences.autostart_auto_receive);
			if autostart_launch && preferences.autostart_auto_broadcast {
				let input_name = input_devices.get(selected_input).cloned();
				service.start_broadcasting(input_name);
			}
		}

		Self {
			local_name,
			local_ip: local_ip.clone(),
			discovery,
			peers: HashMap::new(),
			status: "正在广播并扫描同一局域网的用户...".to_string(),
			last_cleanup: Instant::now(),
			input_devices,
			output_devices,
			selected_input,
			selected_output,
			joined_bands: joined_bands.clone(),
			available_bands: HashMap::new(),
			band_mic_enabled: Arc::clone(&band_mic_enabled),
			available_collapsed,
			new_band_text: String::new(),
			audio_service,
			mic_level: 0.0,
			speaker_level: 0.0,
			tray_icon: None,
			tray_exit_item,
			window_hidden,
			exit_requested: false,
			autostart_enabled: preferences.autostart_enabled,
			autostart_minimize_to_tray: preferences.autostart_minimize_to_tray,
			autostart_auto_broadcast: preferences.autostart_auto_broadcast,
			autostart_auto_receive: preferences.autostart_auto_receive,
		}
	}

	fn ingest_network_events(&mut self) {
		while let Ok(event) = self.discovery.rx.try_recv() {
			match event {
					NetEvent::PeerSeen { ip, name } => {
						if let Some(existing) = self.peers.get_mut(&ip) {
							if let Some(display_name) = normalize_display_name(name) {
								existing.name = display_name;
							}
							existing.last_seen = Instant::now();
						} else {
							self.peers.insert(
								ip.clone(),
								PeerInfo {
									ip,
									name: normalize_display_name(name).unwrap_or_else(|| "未命名用户".to_string()),
									bands: Vec::new(),
									last_seen: Instant::now(),
								},
							);
						}
				}
				NetEvent::BandSeen { ip, name, bands } => {
					if let Some(existing) = self.peers.get_mut(&ip) {
						if let Some(display_name) = normalize_display_name(name) {
							existing.name = display_name;
						}
						existing.bands = bands.clone();
						existing.last_seen = Instant::now();
					} else {
						self.peers.insert(
							ip.clone(),
							PeerInfo {
								ip: ip.clone(),
								name: normalize_display_name(name).unwrap_or_else(|| "未命名用户".to_string()),
								bands: bands.clone(),
								last_seen: Instant::now(),
							},
						);
					}

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

	fn save_preferences(&self) {
		let gate_config = self
			.audio_service
			.as_ref()
			.map(|service| service.mic_gate_config())
			.unwrap_or_default();
		let _ = save_device_preferences(
			Some(self.local_name.clone()),
			self.input_devices.get(self.selected_input).cloned(),
			self.output_devices.get(self.selected_output).cloned(),
			gate_config,
			self.autostart_enabled,
			self.autostart_minimize_to_tray,
			self.autostart_auto_broadcast,
			self.autostart_auto_receive,
		);
	}

	fn apply_autostart_registry(&self) {
		let _ = update_windows_autostart(self.autostart_enabled);
	}

	fn ensure_tray_icon(&mut self) {
		if self.tray_icon.is_some() {
			return;
		}

		let icon = make_tray_icon().unwrap_or_else(|_| fallback_tray_icon());
		let menu = Menu::new();
		let _ = menu.append_items(&[
			&PredefinedMenuItem::separator(),
			&self.tray_exit_item,
		]);
		let tray_icon = TrayIconBuilder::new()
			.with_menu(Box::new(menu))
			.with_menu_on_left_click(false)
			.with_menu_on_right_click(true)
			.with_tooltip("saying 局域网语音聊天")
			.with_icon(icon)
			.build()
			.ok();
		self.tray_icon = tray_icon;
	}

	fn set_window_visible(&mut self, ctx: &egui::Context, visible: bool) {
		self.window_hidden = !visible;
		ctx.send_viewport_cmd(egui::ViewportCommand::Visible(visible));
	}

	fn handle_tray_events(&mut self, ctx: &egui::Context) {
		while let Ok(event) = TrayIconEvent::receiver().try_recv() {
			match event {
				TrayIconEvent::Click {
					button: MouseButton::Left,
					..
				} => {
					let next_visible = self.window_hidden;
					self.set_window_visible(ctx, next_visible);
				}
				TrayIconEvent::DoubleClick { .. } => {
					let next_visible = self.window_hidden;
					self.set_window_visible(ctx, next_visible);
				}
				_ => {}
			}
		}

		while let Ok(event) = MenuEvent::receiver().try_recv() {
			if event.id == self.tray_exit_item.id() {
				self.exit_requested = true;
				ctx.send_viewport_cmd(egui::ViewportCommand::Close);
			}
		}
	}

	fn local_is_speaking_on_band(&self, band: u16) -> bool {
		self.audio_service
			.as_ref()
			.map(|service| service.is_broadcasting())
			.unwrap_or(false)
			&& self.is_band_mic_enabled(band)
			&& self.mic_level > LOCAL_SPEAKING_THRESHOLD
	}
}

impl eframe::App for MyApp {
	fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
		self.ensure_tray_icon();
		self.handle_tray_events(ctx);
		if ctx.input(|input| input.viewport().close_requested()) {
			if self.exit_requested {
				return;
			}
			ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
			self.set_window_visible(ctx, false);
		}
		if self.window_hidden {
			ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
		}

		self.ingest_network_events();

		egui::TopBottomPanel::top("header").show(ctx, |ui| {
			ui.vertical(|ui| {
				ui.heading("局域网语音聊天 - 用户发现");
				ui.horizontal(|ui| {
					ui.label("本机名称:");
					let response = ui.text_edit_singleline(&mut self.local_name);
					if response.changed() {
						self.discovery.set_local_name(self.local_name.clone());
						self.save_preferences();
					}
				});
				ui.label(format!("本机 IP: {}", self.local_ip));
				ui.label(&self.status);
				ui.separator();
				ui.horizontal_wrapped(|ui| {
					let mut autostart_changed = false;
					autostart_changed |= ui
						.checkbox(&mut self.autostart_enabled, "是否开机自启动")
						.changed();
					if ui
						.checkbox(&mut self.autostart_minimize_to_tray, "自启动时最小化到系统托盘")
						.changed()
					{
						self.save_preferences();
					}
					if ui
						.checkbox(&mut self.autostart_auto_broadcast, "自启动时自动开始广播")
						.changed()
					{
						self.save_preferences();
					}
					if ui
						.checkbox(&mut self.autostart_auto_receive, "自启动时自动开始接收声音")
						.changed()
					{
						self.save_preferences();
					}
					if autostart_changed {
						self.apply_autostart_registry();
						self.save_preferences();
					}
				});
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
						self.save_preferences();
					}

					let broadcasting = self
						.audio_service
						.as_ref()
						.map(|service| service.is_broadcasting())
						.unwrap_or(false);

					if ui
						.button(if broadcasting { "重启广播音频" } else { "开始广播音频" })
						.clicked()
					{
						if let Some(service) = self.audio_service.as_mut() {
							let input_name = self.input_devices.get(self.selected_input).cloned();
							service.start_broadcasting(input_name);
							self.status = "音频广播已开始，接收保持开启".to_string();
						}
					}

					let receive_audio_enabled = self
						.audio_service
						.as_ref()
						.map(|service| service.is_receive_audio_enabled())
						.unwrap_or(true);
					if ui
						.button(if receive_audio_enabled { "停止接收所有频道声音" } else { "恢复接收所有频道声音" })
						.clicked()
					{
						if let Some(service) = self.audio_service.as_mut() {
							let next = !receive_audio_enabled;
							service.set_receive_audio_enabled(next);
							self.status = if next {
								"已恢复接收所有频道声音".to_string()
							} else {
								"已停止接收所有频道声音".to_string()
							};
						}
					}

					let loopback_enabled = self
						.audio_service
						.as_ref()
						.map(|service| service.is_loopback_enabled())
						.unwrap_or(false);
					if ui
						.button(if loopback_enabled { "声音回环: 开" } else { "声音回环: 关" })
						.clicked()
					{
						if let Some(service) = self.audio_service.as_mut() {
							let next = !loopback_enabled;
							service.set_loopback_enabled(next);
							self.status = if next {
								"声音回环已开启".to_string()
							} else {
								"声音回环已关闭".to_string()
							};
						}
					}

					if ui
						.add_enabled(broadcasting, egui::Button::new("停止广播音频"))
						.clicked()
					{
						if let Some(service) = self.audio_service.as_mut() {
							service.stop_broadcasting();
							self.status = "音频广播已停止，接收仍保持开启".to_string();
						}
					}
				});

				ui.add_space(8.0);
				ui.separator();
				ui.horizontal_wrapped(|ui| {
					ui.label("麦克风门限");
					if let Some(service) = self.audio_service.as_ref() {
						let mut gate_config = service.mic_gate_config();
						let mut changed = false;
						changed |= ui
							.add(
								egui::Slider::new(&mut gate_config.start_threshold, 0.0..=0.30)
									.text("起播阈值")
							)
							.changed();
						changed |= ui
							.add(
								egui::Slider::new(&mut gate_config.stop_threshold, 0.0..=0.25)
									.text("停播阈值")
							)
							.changed();
						changed |= ui
							.add(
								egui::Slider::new(&mut gate_config.hold_ms, 0..=500)
									.text("保持时间(ms)")
							)
							.changed();
						gate_config = gate_config.normalized();
						if changed {
							service.set_mic_gate_config(gate_config);
							self.save_preferences();
						}
						ui.small(format!(
							"当前: 起播 {:.2}, 停播 {:.2}, 保持 {}ms",
							gate_config.start_threshold,
							gate_config.stop_threshold,
							gate_config.hold_ms
						));
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
				ui.label(format!("已发现 {} 个在线用户", self.peers.values().filter(|peer| peer.ip != self.local_ip).count()));
			});

			ui.add_space(8.0);

			ui.label("加入的频段列表");
			ui.add_space(10.0);
			egui::ScrollArea::vertical().show(ui, |ui| {
				for band in self.joined_bands.clone() {
					ui.group(|ui| {
						ui.label(format!("音频频段: {}", band));
						ui.add_space(10.0);
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
						ui.add_space(10.0);
						ui.horizontal_wrapped(|ui| {
							// local card (auto-size, inner padding 5px)
							let frame = egui::Frame::none()
								.stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200)))
								;

							// fixed-size local card
							let size = egui::vec2(100.0, 60.0);
							let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
							let mut child = ui.child_ui(rect, egui::Layout::top_down(egui::Align::Center));
							frame.show(&mut child, |ui| {
								ui.vertical_centered(|ui| {
									ui.horizontal(|ui| {
										draw_status_dot(ui, self.local_is_speaking_on_band(band));
										ui.label(format!("{}（本机）", self.local_name));
									});
									ui.small(format!("ip: {}", self.local_ip));
									let age = self
										.audio_service
										.as_ref()
										.and_then(|service| service.local_voice_age())
										.unwrap_or_else(|| Duration::from_secs(0));
									ui.small(format!("{} 前", format_age(age)));
								});
							});

							// peers
							for peer in self.peers.values().filter(|peer| peer.ip != self.local_ip && peer.bands.contains(&band)) {
								let speaking = self
									.audio_service
									.as_ref()
									.map(|service| service.peer_is_speaking(band, &peer.ip))
									.unwrap_or(false);
								let frame = egui::Frame::none()
									.stroke(egui::Stroke::new(1.0, if speaking { egui::Color32::from_rgb(46, 204, 113) } else { egui::Color32::from_gray(200) }))
									;

								// fixed-size peer card
								let size = egui::vec2(100.0, 60.0);
								let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::hover());
								let mut child = ui.child_ui(rect, egui::Layout::top_down(egui::Align::Center));
								frame.show(&mut child, |ui| {
									ui.vertical_centered(|ui| {
										ui.horizontal(|ui| {
											draw_status_dot(ui, speaking);
											ui.label(&peer.name);
										});
										ui.small(format!("ip: {}", peer.ip));
										let age = peer.last_seen.elapsed();
										ui.small(format!("{} 前", format_age(age)));
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
			ui.add_space(10.0);
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
						ui.add_space(10.0);
						ui.horizontal_wrapped(|ui| {
							for peer in self.peers.values().filter(|p| p.ip != self.local_ip && p.bands.contains(&band)) {
								let speaking = self
									.audio_service
									.as_ref()
									.map(|service| service.peer_is_speaking(band, &peer.ip))
									.unwrap_or(false);
								let frame = egui::Frame::none()
									.stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(200)))
									;

								frame.show(ui, |ui| {
									ui.vertical_centered(|ui| {
										ui.horizontal(|ui| {
											draw_status_dot(ui, speaking);
											ui.label(&peer.name);
										});
										ui.small(format!("ip: {}", peer.ip));
										let age = peer.last_seen.elapsed();
										ui.small(format!("{} 前", format_age(age)));
									});
								});
							}
						});
					}
				});
				ui.add_space(10.0);
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
	_stop_flag: Arc<AtomicBool>,
	input_stream: Option<cpal::Stream>,
	_output_stream: Option<cpal::Stream>,
	sender_tx: std_mpsc::SyncSender<Vec<u8>>,
	metrics: Arc<AudioMetrics>,
	local_ip: String,
	joined_bands: Arc<Mutex<Vec<u16>>>,
	speaking_map: Arc<Mutex<HashMap<(u16, String), Instant>>>,
	playback_queue: Arc<Mutex<VecDeque<i16>>>,
	output_rate: Option<u32>,
	loopback_enabled: Arc<AtomicBool>,
	receive_audio_enabled: Arc<AtomicBool>,
	local_voice_last_seen: Arc<Mutex<Instant>>,
	mic_gate_config: Arc<Mutex<MicGateConfig>>,
	_band_mic_enabled: Arc<Mutex<HashMap<u16, bool>>>,
}

impl AudioService {
	fn new(
		local_ip: String,
		output_name: Option<String>,
		bands: Vec<u16>,
		band_mic_enabled: Arc<Mutex<HashMap<u16, bool>>>,
		mic_gate_config: Arc<Mutex<MicGateConfig>>,
	) -> Self {
		let stop_flag = Arc::new(AtomicBool::new(true));
		let metrics = Arc::new(AudioMetrics::default());
		let playback_queue = Arc::new(Mutex::new(VecDeque::<i16>::new()));
		let loopback_enabled = Arc::new(AtomicBool::new(false));
		let receive_audio_enabled = Arc::new(AtomicBool::new(true));
		let local_voice_last_seen = Arc::new(Mutex::new(Instant::now()));
		let (snd_tx, snd_rx) = std_mpsc::sync_channel::<Vec<u8>>(64);
		let joined_bands = Arc::new(Mutex::new(bands));
		let bands_for_send = Arc::clone(&joined_bands);
		let band_mic_enabled_for_send = Arc::clone(&band_mic_enabled);

		let speaking_map = Arc::new(Mutex::new(HashMap::new()));


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

		let output_stream = resolve_output_device(&host, output_name.as_deref())
			.and_then(|device| build_output_stream(&device, Arc::clone(&playback_queue)));

		let local_ip_for_recv = local_ip.clone();
		let receiver_stop = Arc::clone(&stop_flag);
		let receiver_queue = Arc::clone(&playback_queue);
		let receiver_metrics = Arc::clone(&metrics);
		let receiver_enabled = Arc::clone(&receive_audio_enabled);
		let bands_for_recv = Arc::clone(&joined_bands);
		let speaking_map_for_recv = Arc::clone(&speaking_map);
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

				for (i, socket) in sockets.iter().enumerate() {
					let band = current_bands.get(i).cloned().unwrap_or(DEFAULT_BAND_PORT);
					match socket.recv_from(&mut buffer) {
						Ok((length, addr)) => {
							if !receiver_enabled.load(Ordering::Relaxed) {
								continue;
							}
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

								// record that we heard audio from this source on this band
								if let Ok(mut map) = speaking_map_for_recv.lock() {
									map.insert((band, source_ip.clone()), Instant::now());
								}
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
			_stop_flag: stop_flag,
			input_stream: None,
			_output_stream: output_stream,
			sender_tx: snd_tx,
			metrics,
			local_ip,
			joined_bands,
			speaking_map,
			playback_queue,
			output_rate,
			loopback_enabled,
				receive_audio_enabled,
				local_voice_last_seen,
			mic_gate_config,
			_band_mic_enabled: band_mic_enabled,
		}
	}

	fn start_broadcasting(&mut self, input_name: Option<String>) {
		if self.input_stream.is_some() {
			return;
		}

		let host = cpal::default_host();
		self.input_stream = input_name.and_then(|name| {
			resolve_input_device(&host, Some(&name)).and_then(|device| {
				build_input_stream(
					&device,
					self.sender_tx.clone(),
					Arc::clone(&self.metrics),
					self.local_ip.clone(),
					Arc::clone(&self.loopback_enabled),
					self.output_rate,
					Arc::clone(&self.playback_queue),
					Arc::clone(&self.mic_gate_config),
					Arc::clone(&self.local_voice_last_seen),
				)
			})
		});
	}

	fn stop_broadcasting(&mut self) {
		let _ = self.input_stream.take();
	}

	fn set_bands(&self, bands: Vec<u16>) {
		if let Ok(mut current) = self.joined_bands.lock() {
			*current = bands;
		}
	}

	fn is_broadcasting(&self) -> bool {
		self.input_stream.is_some()
	}

	fn mic_level(&self) -> f32 {
		self.metrics.mic_level()
	}

	fn speaker_level(&self) -> f32 {
		self.metrics.speaker_level()
	}

	fn set_loopback_enabled(&self, enabled: bool) {
		self.loopback_enabled.store(enabled, Ordering::Relaxed);
	}

	fn is_loopback_enabled(&self) -> bool {
		self.loopback_enabled.load(Ordering::Relaxed)
	}

	fn set_receive_audio_enabled(&self, enabled: bool) {
		self.receive_audio_enabled.store(enabled, Ordering::Relaxed);
		if !enabled {
			if let Ok(mut queue) = self.playback_queue.lock() {
				queue.clear();
			}
			self.metrics.set_speaker_level(0.0);
		}
	}

	fn is_receive_audio_enabled(&self) -> bool {
		self.receive_audio_enabled.load(Ordering::Relaxed)
	}

	fn local_voice_age(&self) -> Option<Duration> {
		self.local_voice_last_seen.lock().ok().map(|instant| instant.elapsed())
	}

	fn mic_gate_config(&self) -> MicGateConfig {
		self.mic_gate_config
			.lock()
			.map(|config| *config)
			.unwrap_or_default()
	}

	fn set_mic_gate_config(&self, config: MicGateConfig) {
		if let Ok(mut current) = self.mic_gate_config.lock() {
			*current = config.normalized();
		}
	}

	// Determine whether a remote peer is currently speaking on a band.
	// Returns true if we've heard audio from (band, peer_ip) within the
	// last 500ms to avoid toggling during short gaps in speech.
	fn peer_is_speaking(&self, band: u16, peer_ip: &str) -> bool {
		if let Ok(map) = self.speaking_map.lock() {
			if let Some(ts) = map.get(&(band, peer_ip.to_string())) {
				return ts.elapsed() <= Duration::from_millis(500);
			}
		}
		false
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
	loopback_enabled: Arc<AtomicBool>,
	output_rate: Option<u32>,
	playback_queue: Arc<Mutex<VecDeque<i16>>>,
	mic_gate_config: Arc<Mutex<MicGateConfig>>,
	local_voice_last_seen: Arc<Mutex<Instant>>,
) -> Option<cpal::Stream> {
	let config = device.default_input_config().ok()?;
	let sample_rate = config.sample_rate().0;
	let channels = config.channels() as usize;
	let err_fn = |error| eprintln!("input stream error: {error}");
	let broadcast_active = Arc::new(AtomicBool::new(false));
	let last_voice = Arc::clone(&local_voice_last_seen);

	let stream = match config.sample_format() {
		SampleFormat::F32 => device.build_input_stream(
			&config.clone().into(),
			{
				let sender = sender.clone();
				let metrics = Arc::clone(&metrics);
				let local_id = local_id.clone();
				let loopback_enabled = Arc::clone(&loopback_enabled);
				let playback_queue = Arc::clone(&playback_queue);
				let broadcast_active = Arc::clone(&broadcast_active);
				let last_voice = Arc::clone(&last_voice);
				let mic_gate_config = Arc::clone(&mic_gate_config);
				move |data: &[f32], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					let sample = frame[0].clamp(-1.0, 1.0);
					samples.push((sample * i16::MAX as f32) as i16);
				}
				let level = audio_level_i16(&samples);
				metrics.set_mic_level(level);
				if let Ok(config) = mic_gate_config.lock() {
					if should_broadcast_input(
						level,
						&broadcast_active,
						&last_voice,
						config.start_threshold,
						config.stop_threshold,
						config.hold_duration(),
					) {
						broadcast_active.store(true, Ordering::Relaxed);
						let packet = build_audio_packet(&local_id, sample_rate, &samples);
						let _ = sender.try_send(packet);
						if loopback_enabled.load(Ordering::Relaxed) {
							metrics.set_speaker_level(level);
							let loopback_samples = match output_rate {
								Some(target_rate) if target_rate != sample_rate => {
									resample_i16_mono(&samples, sample_rate, target_rate)
								}
								_ => samples.clone(),
							};
							if let Ok(mut queue) = playback_queue.lock() {
								queue.extend(loopback_samples);
							}
						}
					} else {
						broadcast_active.store(false, Ordering::Relaxed);
					}
				}
			}
			},
			err_fn,
			None,
		),
		SampleFormat::I16 => device.build_input_stream(
			&config.clone().into(),
			{
				let sender = sender.clone();
				let metrics = Arc::clone(&metrics);
				let local_id = local_id.clone();
				let loopback_enabled = Arc::clone(&loopback_enabled);
				let playback_queue = Arc::clone(&playback_queue);
				let broadcast_active = Arc::clone(&broadcast_active);
				let last_voice = Arc::clone(&last_voice);
				let mic_gate_config = Arc::clone(&mic_gate_config);
				move |data: &[i16], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					samples.push(frame[0]);
				}
				let level = audio_level_i16(&samples);
				metrics.set_mic_level(level);
				if let Ok(config) = mic_gate_config.lock() {
					if should_broadcast_input(
						level,
						&broadcast_active,
						&last_voice,
						config.start_threshold,
						config.stop_threshold,
						config.hold_duration(),
					) {
						broadcast_active.store(true, Ordering::Relaxed);
						let packet = build_audio_packet(&local_id, sample_rate, &samples);
						let _ = sender.try_send(packet);
						if loopback_enabled.load(Ordering::Relaxed) {
							metrics.set_speaker_level(level);
							let loopback_samples = match output_rate {
								Some(target_rate) if target_rate != sample_rate => {
									resample_i16_mono(&samples, sample_rate, target_rate)
								}
								_ => samples.clone(),
							};
							if let Ok(mut queue) = playback_queue.lock() {
								queue.extend(loopback_samples);
							}
						}
					} else {
						broadcast_active.store(false, Ordering::Relaxed);
					}
				}
			}
			},
			err_fn,
			None,
		),
		SampleFormat::U16 => device.build_input_stream(
			&config.clone().into(),
			{
				let sender = sender.clone();
				let metrics = Arc::clone(&metrics);
				let local_id = local_id.clone();
				let loopback_enabled = Arc::clone(&loopback_enabled);
				let playback_queue = Arc::clone(&playback_queue);
				let broadcast_active = Arc::clone(&broadcast_active);
				let last_voice = Arc::clone(&last_voice);
				let mic_gate_config = Arc::clone(&mic_gate_config);
				move |data: &[u16], _| {
				let mut samples = Vec::with_capacity(data.len() / channels + 1);
				for frame in data.chunks(channels) {
					samples.push((frame[0] as i32 - 32768) as i16);
				}
				let level = audio_level_i16(&samples);
				metrics.set_mic_level(level);
				if let Ok(config) = mic_gate_config.lock() {
					if should_broadcast_input(
						level,
						&broadcast_active,
						&last_voice,
						config.start_threshold,
						config.stop_threshold,
						config.hold_duration(),
					) {
						broadcast_active.store(true, Ordering::Relaxed);
						let packet = build_audio_packet(&local_id, sample_rate, &samples);
						let _ = sender.try_send(packet);
						if loopback_enabled.load(Ordering::Relaxed) {
							metrics.set_speaker_level(level);
							let loopback_samples = match output_rate {
								Some(target_rate) if target_rate != sample_rate => {
									resample_i16_mono(&samples, sample_rate, target_rate)
								}
								_ => samples.clone(),
							};
							if let Ok(mut queue) = playback_queue.lock() {
								queue.extend(loopback_samples);
							}
						}
					} else {
						broadcast_active.store(false, Ordering::Relaxed);
					}
				}
			}
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

fn should_broadcast_input(
	level: f32,
	broadcast_active: &Arc<AtomicBool>,
	last_voice: &Arc<Mutex<Instant>>,
	start_threshold: f32,
	stop_threshold: f32,
	hold_duration: Duration,
) -> bool {
	let active = broadcast_active.load(Ordering::Relaxed);
	if level >= start_threshold {
		if let Ok(mut last) = last_voice.lock() {
			*last = Instant::now();
		}
		broadcast_active.store(true, Ordering::Relaxed);
		return true;
	}

	if active {
		if level >= stop_threshold {
			if let Ok(mut last) = last_voice.lock() {
				*last = Instant::now();
			}
			return true;
		}

		if let Ok(last) = last_voice.lock() {
			if last.elapsed() <= hold_duration {
				return true;
			}
		}
	}

	broadcast_active.store(false, Ordering::Relaxed);
	false
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
								let peer_ip = if packet.ip.is_empty() { addr.ip().to_string() } else { packet.ip };
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
							let peer_ip = if packet.ip.is_empty() { addr.ip().to_string() } else { packet.ip };
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
		ip: parts.next()?.to_string(),
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

	let ip = parts.next()?.to_string();
	let name = parts.next()?.to_string();
	let bands = parts
		.next()?
		.split(',')
		.filter_map(|band| band.trim().parse::<u16>().ok())
		.collect::<Vec<_>>();

	Some(BandDiscoveryPacket { ip, name, bands })
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

fn normalize_display_name(value: String) -> Option<String> {
	let trimmed = value.trim();
	if trimmed.is_empty() {
		None
	} else {
		Some(trimmed.to_string())
	}
}

fn draw_status_dot(ui: &mut egui::Ui, active: bool) {
	let size = egui::vec2(18.0, 18.0);
	let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
	let painter = ui.painter_at(rect);
	let center = rect.center();

	if active {
		painter.circle_filled(center, 3.5, egui::Color32::from_rgb(46, 204, 113));
	} else {
		painter.circle_filled(center, 3.5, egui::Color32::from_gray(120));
	}
}

struct DiscoveryPacket {
	ip: String,
	name: String,
}

struct BandDiscoveryPacket {
	ip: String,
	name: String,
	bands: Vec<u16>,
}
