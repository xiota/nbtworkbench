use std::{
	ffi::OsStr,
	fmt::Display,
	io::Read,
	path::{Path, PathBuf},
	time::Duration,
};

use anyhow::{Context, Result, anyhow, ensure};
use compact_str::CompactString;
use flate2::Compression;
use thiserror::Error;
use winit::dpi::PhysicalSize;
use zune_inflate::DeflateDecoder;

use crate::{
	elements::{
		ComplexNbtElementVariant, NbtElementVariant,
		array::{NbtByteArray, NbtIntArray, NbtLongArray},
		byte::NbtByte,
		chunk::NbtChunk,
		compound::NbtCompound,
		double::NbtDouble,
		element::NbtElement,
		float::NbtFloat,
		int::NbtInt,
		list::NbtList,
		long::NbtLong,
		region::NbtRegion,
		short::NbtShort,
		string::NbtString,
	},
	history::{WorkbenchAction, manager::HistoryMananger},
	render::{
		RenderContext,
		assets::{
			BASE_Z, CONNECTION_UV, FROM_CLIPBOARD_GHOST_UV, FROM_CLIPBOARD_UV, GZIP_FILE_TYPE_UV, HEADER_SIZE, HELD_SCROLLBAR_UV, JUST_OVERLAPPING_BASE_Z, LINE_NUMBER_SEPARATOR_UV, LITTLE_ENDIAN_HEADER_NBT_FILE_TYPE_UV,
			LITTLE_ENDIAN_NBT_FILE_TYPE_UV, MCA_FILE_TYPE_UV, NBT_FILE_TYPE_UV, SCROLLBAR_Z, SNBT_FILE_TYPE_UV, STEAL_ANIMATION_OVERLAY_UV, UNHELD_SCROLLBAR_UV, ZLIB_FILE_TYPE_UV, ZOffset,
		},
		color::TextColor,
		vertex_buffer_builder::VertexBufferBuilder,
		widget::{
			selected_text::{SaveSelectedTextError, SelectedText, SelectedTextConstructionError},
			text::{TEXT_DOUBLE_CLICK_INTERVAL, get_cursor_left_jump_idx, get_cursor_right_jump_idx},
		},
	},
	util::{StrExt, Timestamp, Vec2u, drop_on_separate_thread},
	workbench::{
		marked_line::MarkedLines,
		FileUpdateSubscription, HeldEntry,
	},
};

pub mod manager;

pub struct Tab {
	pub root: NbtElement,
	pub path: FilePath,
	pub format: NbtFileFormat,

	pub history: HistoryMananger,

	pub bookmarks: MarkedLines,
	pub subscription: Option<FileUpdateSubscription>,
	pub selected_text: Option<SelectedText>,

	pub held_entry: Option<HeldEntry>,

	pub freehand_mode: bool,

	pub scroll: usize,
	pub horizontal_scroll: usize,

	pub window_dims: PhysicalSize<u32>,

	pub last_close_attempt: Timestamp,
	// todo: change to own type
	pub last_selected_text_interaction: (usize, usize, Timestamp),
	pub last_interaction: Timestamp,
	// todo: change to own type
	pub last_double_click_interaction: (usize, Timestamp),
	// todo: refactor to own type with OwnedIndices instead of Vec2u
	pub steal_animation_data: Option<(Timestamp, Vec2u)>,
}

impl Tab {
	pub const FILE_TYPE_FILTERS: &'static [(&'static str, &'static [&'static str])] = &[
		("Uncompressed NBT File", &["nbt"]),
		("SNBT File", &["snbt"]),
		("Region File", &["mca", "mcr"]),
		("Compressed NBT File", &["dat", "dat_old", "dat_new", "dat_mcr", "old", "schem", "schematic", "litematic"]),
		("Little Endian NBT File", &["nbt", "mcstructure"]),
		("Little Endian NBT File (With Header)", &["dat"]),
	];
	pub const AUTOSAVE_INTERVAL: Duration = Duration::from_secs(30);
	pub const TAB_CLOSE_DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(2_000);
	pub const AUTOSAVE_MAXIMUM_LINES: usize = 1_000_000;

	pub fn new(nbt: NbtElement, path: FilePath, format: NbtFileFormat, window_dims: PhysicalSize<u32>) -> Result<Self> {
		ensure!(nbt.is_compound() || nbt.is_list(), "Parsed NBT was not a Compound or List");

		Ok(Self {
			root: nbt,
			path,
			format,

			history: HistoryMananger::new(),

			bookmarks: MarkedLines::new(),
			subscription: None,
			selected_text: None,

			held_entry: None,

			freehand_mode: false,

			scroll: 0,
			horizontal_scroll: 0,

			window_dims,

			last_close_attempt: Timestamp::UNIX_EPOCH,
			last_selected_text_interaction: (0, 0, Timestamp::UNIX_EPOCH),
			last_interaction: Timestamp::now(),
			last_double_click_interaction: (0, Timestamp::UNIX_EPOCH),
			steal_animation_data: None,
		})
	}

	#[must_use]
	pub fn new_empty_tab(region: bool, window_dims: PhysicalSize<u32>) -> Self {
		Self {
			root: if region { NbtElement::Region(NbtRegion::default()) } else { NbtElement::Compound(NbtCompound::default()) },
			path: FilePath::new("new.nbt").expect("Valid file path"),
			format: if region { NbtFileFormat::Nbt } else { NbtFileFormat::Mca },

			history: HistoryMananger::new(),

			bookmarks: MarkedLines::new(),
			subscription: None,
			selected_text: None,

			held_entry: None,

			freehand_mode: false,

			scroll: 0,
			horizontal_scroll: 0,

			window_dims,

			last_close_attempt: Timestamp::UNIX_EPOCH,
			last_selected_text_interaction: (0, 0, Timestamp::UNIX_EPOCH),
			last_interaction: Timestamp::now(),
			last_double_click_interaction: (0, Timestamp::UNIX_EPOCH),
			steal_animation_data: None,
		}
	}

	pub fn save_selected_text(&mut self) -> Result<(), SaveSelectedTextError> {
		if let Some(action) = WorkbenchAction::bulk(self.selected_text.iter_mut().map(|text| text.save(&mut self.root, &mut self.path)).collect::<Result<Vec<WorkbenchAction>, SaveSelectedTextError>>()?) {
			self.history.append(action);
		}
		Ok(())
	}

	#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
	pub fn save(&mut self, force_dialog: bool) -> Result<()> {
		self.save_selected_text()?;
		if !force_dialog {
			std::fs::write(&self.path, self.format.encode(&self.root))?;
			self.history.on_save();
			Ok(())
		} else {
			let initial_index = match self.format {
				NbtFileFormat::Nbt => 0,
				NbtFileFormat::Snbt => 1,
				NbtFileFormat::Mca => 2,
				NbtFileFormat::Gzip | NbtFileFormat::Zlib => 3,
				NbtFileFormat::LittleEndianNbt => 4,
				NbtFileFormat::LittleEndianHeaderNbt => 5,
			};
			let dialog = native_dialog::FileDialogBuilder::default()
				.add_filter(Self::FILE_TYPE_FILTERS[initial_index].0, Self::FILE_TYPE_FILTERS[initial_index].1)
				.add_filters(
					Self::FILE_TYPE_FILTERS
						.iter()
						.copied()
						.map(|(a, b)| (a.to_owned(), b.iter().map(|x| x.to_string()).collect::<Vec<_>>()))
						.enumerate()
						.filter(|(idx, _)| *idx != initial_index)
						.map(|(_, x)| x),
				)
				.save_single_file();
			let Ok(Some(path)) = dialog.show() else { return Ok(()) };
			std::fs::write(&path, self.format.encode(&self.root))?;
			self.path.set_path(path)?;
			self.history.on_save();
			Ok(())
		}
	}

	#[cfg(target_arch = "wasm32")]
	pub fn save(&mut self, _: bool) -> Result<()> {
		self.save_selected_text()?;
		let bytes = self.format.encode(&self.root);
		crate::wasm::save(self.name.as_ref(), bytes);
		self.history.on_save();
		Ok(())
	}

	pub fn render(&self, builder: &mut VertexBufferBuilder, ctx: &mut RenderContext, held: bool, skip_tooltips: bool, steal_delta: f32) {
		let TabConstants { horizontal_scroll, scroll, .. } = self.consts();
		let horizontal_scroll_before = core::mem::replace(&mut builder.horizontal_scroll, horizontal_scroll);
		// let start = std::time::Instant::now();
		{
			let mut remaining_scroll = builder.scroll() / 16;
			if remaining_scroll == 0 {
				builder.draw_texture(ctx.pos() - (16, 0), CONNECTION_UV, (16, 9));
			}
			self.root.render(&mut remaining_scroll, builder, Some(&self.path.name()), true, ctx);
		}
		// println!("Tree Only: {}ms", start.elapsed().as_millis_f64());
		builder.color = TextColor::White.to_raw();
		if self.root.as_region().is_some_and(|region| region.is_grid_layout()) {
			ctx.render_grid_line_numbers(builder, &self.bookmarks);
		} else {
			ctx.render_line_numbers(builder, &self.bookmarks);
		}
		ctx.render_key_value_errors(builder);
		builder.horizontal_scroll = horizontal_scroll_before;

		if builder.window_height() >= HEADER_SIZE {
			let height = self.root.height() * 16;
			let total = builder.window_height() - HEADER_SIZE;
			if height > total & !15 {
				let scrollbar_height = (total & !15) * total / height;
				let offset = total * scroll / height + HEADER_SIZE;
				let held = ((builder.window_width() - 8)..builder.window_width()).contains(&ctx.mouse.x) && (offset..=(offset + scrollbar_height)).contains(&ctx.mouse.y) || held;
				let uv = if held { HELD_SCROLLBAR_UV } else { UNHELD_SCROLLBAR_UV };
				builder.draw_texture_z((builder.window_width() - 7, offset), SCROLLBAR_Z, uv, (6, 1));
				if scrollbar_height > 2 {
					builder.draw_texture_region_z((builder.window_width() - 7, offset + 1), SCROLLBAR_Z, uv + (0, 5), (6, scrollbar_height.saturating_sub(1)), (6, 4));
				}
				if scrollbar_height > 1 {
					builder.draw_texture_z((builder.window_width() - 7, offset + scrollbar_height), SCROLLBAR_Z, uv + (0, 15), (6, 1));
				}
			}
		}

		if self.root.as_region().is_none_or(|region| !region.is_grid_layout()) {
			ctx.render_scrollbar_bookmarks(builder, &self.bookmarks, &self.root);
		}

		// shifted one left to center between clipboard and freehand
		builder.draw_texture_region_z((260, 22), BASE_Z, LINE_NUMBER_SEPARATOR_UV, (2, 23), (2, 16));

		{
			let mx = ((24..46).contains(&ctx.mouse.y) && ctx.mouse.x >= 16 + 16 + 4).then(|| (ctx.mouse.x - (16 + 16 + 4)) & !15);
			for (idx, (selected, unselected, name)) in [
				(NbtByte::UV, NbtByte::GHOST_UV, "Byte (1)"),
				(NbtShort::UV, NbtShort::GHOST_UV, "Short (2)"),
				(NbtInt::UV, NbtInt::GHOST_UV, "Int (3)"),
				(NbtLong::UV, NbtLong::GHOST_UV, "Long (4)"),
				(NbtFloat::UV, NbtFloat::GHOST_UV, "Float (5)"),
				(NbtDouble::UV, NbtDouble::GHOST_UV, "Double (6)"),
				(NbtByteArray::UV, NbtByteArray::GHOST_UV, "Byte Array (7)"),
				(NbtIntArray::UV, NbtIntArray::GHOST_UV, "Int Array (8)"),
				(NbtLongArray::UV, NbtLongArray::GHOST_UV, "Long Array (9)"),
				(NbtString::UV, NbtString::GHOST_UV, "String (0)"),
				(NbtList::UV, NbtList::GHOST_UV, "List (-)"),
				(NbtCompound::UV, NbtCompound::GHOST_UV, "Compound (=)"),
			]
			.into_iter()
			.enumerate()
			{
				let uv = if mx == Some(idx * 16) && !skip_tooltips {
					builder.draw_tooltip(&[name], (idx * 16 + 16 + 16 + 4, 26 + 16), false);
					selected
				} else {
					unselected
				};

				builder.draw_texture((idx * 16 + 16 + 16 + 4, 26), uv, (16, 16));
			}

			{
				let uv = if mx == Some(192) && self.root.is_region() && !skip_tooltips {
					builder.draw_tooltip(&["Chunk (`)"], (192, 26 + 16), false);
					NbtChunk::UV
				} else {
					NbtChunk::GHOST_UV
				};
				builder.draw_texture((192 + 16 + 16 + 4, 26), uv, (16, 16));
			}

			{
				let uv = if mx == Some(208) && !skip_tooltips {
					builder.draw_tooltip(&["Clipboard (V)"], (208, 26 + 16), false);
					FROM_CLIPBOARD_UV
				} else {
					FROM_CLIPBOARD_GHOST_UV
				};
				builder.draw_texture((208 + 16 + 16 + 4, 26), uv, (16, 16));
			}
		}

		if steal_delta > 0.0 {
			let y = ((ctx.mouse.y - HEADER_SIZE) & !15) + HEADER_SIZE;
			let height = (16.0 * steal_delta).round() as usize;
			builder.draw_texture_region_z(
				(ctx.left_margin() - 2, y + (16 - height)),
				JUST_OVERLAPPING_BASE_Z,
				STEAL_ANIMATION_OVERLAY_UV,
				(builder.window_width() + 2 - ctx.left_margin(), height),
				(16, 16),
			);
		}
	}

	pub fn draw_icon(&self, builder: &mut VertexBufferBuilder, pos: impl Into<Vec2u>, z: ZOffset) {
		if self.root.is_compound() {
			builder.draw_texture_z(pos, z, NbtCompound::ROOT_UV, (16, 16));
		} else if self.root.is_region() {
			builder.draw_texture_z(pos, z, NbtRegion::UV, (16, 16));
		} else if self.root.is_list() {
			builder.draw_texture_z(pos, z, NbtList::UV, (16, 16));
		}
	}

	pub fn set_selected_text_with_doubleclick(&mut self, result: Result<SelectedText, SelectedTextConstructionError>) -> Result<(), SelectedTextConstructionError> {
		let now = Timestamp::now();
		match result {
			Ok(mut text) => {
				let (old_y, times_clicked, timestamp) = core::mem::replace(&mut self.last_selected_text_interaction, (text.y, 0, now));
				if timestamp.elapsed() <= TEXT_DOUBLE_CLICK_INTERVAL && old_y == text.y && !text.value.is_empty() {
					self.last_selected_text_interaction = (text.y, times_clicked + 1, now);
					// the previous click count was divisible by 1
					let (left, right) = if times_clicked % 2 == 1 {
						(0, text.value.len())
					} else {
						(get_cursor_left_jump_idx(text.cursor, text.value.as_bytes()), get_cursor_right_jump_idx(text.cursor, text.value.as_bytes()))
					};
					if right > left {
						text.selection = Some(left);
					}
					text.cursor = right;
				}
				Ok(())
			}
			Err(e) => {
				self.selected_text = None;
				self.last_selected_text_interaction = (0, 0, Timestamp::UNIX_EPOCH);
				Err(e)
			}
		}
	}

	pub fn refresh_selected_text_horizontal_scroll(&mut self) {
		let TabConstants { left_margin, horizontal_scroll, .. } = self.consts();

		let free_space = 48 + left_margin;
		if let Some(selected_text) = self.selected_text.as_ref() {
			let pos = left_margin + selected_text.indices.len() * 16 + 32 + SelectedText::PREFIXING_SPACE_WIDTH + selected_text.prefix.0.width() + selected_text.keyfix.as_ref().map_or(0, |x| x.0.width()) + selected_text.value.split_at(selected_text.cursor).0.width();
			if pos + free_space < self.window_dims.width as usize {
				self.horizontal_scroll = 0;
			} else if pos + free_space >= self.window_dims.width as usize + horizontal_scroll {
				self.horizontal_scroll = pos + free_space - self.window_dims.width as usize;
			} else if pos < horizontal_scroll + free_space {
				self.horizontal_scroll = pos.saturating_sub(free_space);
			}
			self.modify_horizontal_scroll(|x| x)
		}
	}

	pub fn refresh_scrolls(&mut self) {
		self.modify_scroll(|x| x);
		self.modify_horizontal_scroll(|x| x);
	}

	pub fn set_window_dims(&mut self, window_dims: PhysicalSize<u32>) {
		self.window_dims = window_dims;
		self.refresh_scrolls();
	}

	// todo: make all actions refresh scrolls so that these fields can simply be read from without the getters and so that this becomes redundant
	#[must_use]
	pub fn consts(&self) -> TabConstants {
		TabConstants {
			left_margin: self.left_margin(),
			scroll: self.scroll(),
			horizontal_scroll: self.horizontal_scroll(),
		}
	}

	#[deprecated = "Use `Tab::consts`"]
	#[must_use]
	pub fn left_margin(&self) -> usize { ((self.root.true_height() + self.held_entry.as_ref().map_or(0, |held_entry| held_entry.kv.1.true_height())).ilog10() as usize + 1) * 8 + 4 + 8 }

	pub fn modify_scroll(&mut self, f: impl FnOnce(usize) -> usize) {
		self.scroll = f(self.scroll);
		self.scroll = self.scroll();
	}

	#[deprecated = "Use `Tab::consts`"]
	#[must_use]
	pub fn scroll(&self) -> usize {
		let height = self.root.height() * 16 + 32 + 15;
		let scroll = self.scroll;
		let max = (height + HEADER_SIZE).saturating_sub(self.window_dims.width as usize);
		scroll.min(max) & !15
	}

	#[deprecated = "Use `Tab::consts`"]
	#[must_use]
	pub fn horizontal_scroll(&self) -> usize {
		let left_margin = self.left_margin();
		let selected_text_width = if let Some(selected_text) = &self.selected_text {
			selected_text.indices.len() * NbtElement::ICON_WIDTH + NbtElement::TOGGLE_WIDTH + NbtElement::ICON_WIDTH + SelectedText::PREFIXING_SPACE_WIDTH + selected_text.width()
		} else {
			0
		};
		let width = self.root.end_x().max(self.path.name().width()).max(selected_text_width) + 32 + 48;
		let scroll = self.horizontal_scroll;
		let max = (width + left_margin).saturating_sub(self.window_dims.width as usize);
		scroll.min(max)
	}

	pub fn modify_horizontal_scroll(&mut self, f: impl FnOnce(usize) -> usize) {
		self.horizontal_scroll = f(self.horizontal_scroll);
		self.horizontal_scroll = self.horizontal_scroll();
	}

	#[must_use]
	pub fn end_x(&self) -> usize {
		let TabConstants { left_margin, .. } = self.consts();
		let selected_text_width = self.selected_text.as_ref().map_or(0, |text| text.end_x(left_margin));
		let root_width = left_margin + self.root.end_x().max(NbtElement::TOGGLE_WIDTH + NbtElement::ICON_WIDTH + SelectedText::PREFIXING_SPACE_WIDTH + self.path.name().width());
		usize::max(selected_text_width, root_width)
	}

	pub fn on_scroll(&mut self, scroll: f32) {
		#[cfg(target_os = "macos")]
		const SCROLL_MULTIPLIER: f32 = 4.0;
		#[cfg(not(target_os = "macos"))]
		const SCROLL_MULTIPLIER: f32 = 48.0;

		if scroll.is_sign_negative() && self.scroll < (scroll * -SCROLL_MULTIPLIER) as usize {
			self.scroll = 0;
		} else if scroll.is_sign_negative() {
			self.scroll -= (scroll * -SCROLL_MULTIPLIER) as usize;
		} else {
			self.scroll += (scroll * SCROLL_MULTIPLIER) as usize;
		}
		self.scroll = self.scroll();
	}

	pub fn on_horizontal_scroll(&mut self, scroll: f32) {
		#[cfg(target_os = "macos")]
		const SCROLL_MULTIPLIER: f32 = 4.0;
		#[cfg(not(target_os = "macos"))]
		const SCROLL_MULTIPLIER: f32 = 48.0;

		if scroll.is_sign_negative() && self.horizontal_scroll < (scroll * -SCROLL_MULTIPLIER) as usize {
			self.horizontal_scroll = 0;
		} else if scroll.is_sign_negative() {
			self.horizontal_scroll -= (scroll * -SCROLL_MULTIPLIER) as usize;
		} else {
			self.horizontal_scroll += (scroll * SCROLL_MULTIPLIER) as usize;
		}
		self.modify_horizontal_scroll(|x| x);
	}

	pub fn parse_raw(path: impl AsRef<Path>, buf: Vec<u8>) -> Result<(NbtElement, NbtFileFormat)> {
		let path = path.as_ref();
		Ok(if let Some("mca" | "mcr") = path.extension().and_then(OsStr::to_str) {
			(NbtElement::from_be_mca(buf.as_slice()).context("Failed to parse MCA file")?, NbtFileFormat::Mca)
		} else if let Some(0x1F8B) = buf.first_chunk::<2>().copied().map(u16::from_be_bytes) {
			(
				NbtElement::from_be_file(&DeflateDecoder::new(buf.as_slice()).decode_gzip().context("Failed to decode gzip compressed NBT")?).context("Failed to parse NBT")?,
				NbtFileFormat::Gzip,
			)
		} else if let Some(0x7801 | 0x789C | 0x78DA) = buf.first_chunk::<2>().copied().map(u16::from_be_bytes) {
			(
				NbtElement::from_be_file(&DeflateDecoder::new(buf.as_slice()).decode_zlib().context("Failed to decode zlib compressed NBT")?).context("Failed to parse NBT")?,
				NbtFileFormat::Zlib,
			)
		} else if let result = NbtElement::from_be_file(buf.as_slice()).context("Tried to parse uncompressed NBT")
			&& {
				#[cfg(debug_assertions)]
				if result.is_err() {
					crate::error!("{result:?}");
				}
				true
			} && let Ok(nbt) = result
		{
			(nbt, NbtFileFormat::Nbt)
		} else if let result = NbtElement::from_le_file(buf.as_slice()).context("Tried to parse uncompressed little-endian NBT")
			&& {
				#[cfg(debug_assertions)]
				if result.is_err() {
					crate::error!("{result:?}");
				}
				true
			} && let Ok((nbt, header)) = result
		{
			(nbt, if header { NbtFileFormat::LittleEndianHeaderNbt } else { NbtFileFormat::LittleEndianNbt })
		} else {
			(
				core::str::from_utf8(&buf)
					.ok()
					.and_then(|s| NbtElement::from_str(s).ok())
					.context(anyhow!("Failed to find file type for file {}", path.file_name().unwrap_or(&OsStr::new("")).to_string_lossy()))?
					.1,
				NbtFileFormat::Snbt,
			)
		})
	}

	#[cfg(not(target_arch = "wasm32"))]
	pub fn refresh(&mut self) -> Result<()> {
		if self.history.has_unsaved_changes() && core::mem::replace(&mut self.last_close_attempt, Timestamp::now()).elapsed() > Self::TAB_CLOSE_DOUBLE_CLICK_INTERVAL {
			return Ok(());
		}

		let bytes = std::fs::read(&self.path)?;
		let (value, format) = Tab::parse_raw(&self.path, bytes)?;

		self.bookmarks.clear();
		self.scroll = 0;
		self.format = format;
		let history = core::mem::replace(&mut self.history, HistoryMananger::new());
		self.selected_text = None;
		self.subscription = None;
		self.last_close_attempt = Timestamp::UNIX_EPOCH;
		let root = core::mem::replace(&mut self.root, value);
		drop_on_separate_thread((root, history));

		Ok(())
	}

	#[cfg(target_arch = "wasm32")]
	pub fn refresh(&mut self) -> Result<()> { Ok(()) }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum NbtFileFormat {
	Nbt,
	Gzip,
	Zlib,
	Snbt,
	LittleEndianNbt,
	LittleEndianHeaderNbt,

	Mca,
}

impl NbtFileFormat {
	#[must_use]
	pub const fn cycle(self) -> Self {
		match self {
			Self::Nbt => Self::Gzip,
			Self::Gzip => Self::Zlib,
			Self::Zlib => Self::LittleEndianNbt,
			Self::LittleEndianNbt => Self::LittleEndianHeaderNbt,
			Self::LittleEndianHeaderNbt => Self::Snbt,
			Self::Snbt => Self::Nbt,

			Self::Mca => Self::Mca,
		}
	}

	#[must_use]
	pub const fn rev_cycle(self) -> Self {
		match self {
			Self::Nbt => Self::Snbt,
			Self::Gzip => Self::Nbt,
			Self::Zlib => Self::Gzip,
			Self::LittleEndianNbt => Self::Zlib,
			Self::LittleEndianHeaderNbt => Self::LittleEndianNbt,
			Self::Snbt => Self::LittleEndianHeaderNbt,

			Self::Mca => Self::Mca,
		}
	}

	#[must_use]
	pub fn encode(self, data: &NbtElement) -> Vec<u8> {
		match self {
			Self::Nbt | Self::Mca => data.to_be_file(),
			Self::Gzip => {
				let mut vec = vec![];
				let _ = flate2::read::GzEncoder::new(data.to_be_file().as_slice(), Compression::best()).read_to_end(&mut vec);
				vec
			}
			Self::Zlib => {
				let mut vec = vec![];
				let _ = flate2::read::ZlibEncoder::new(data.to_be_file().as_slice(), Compression::best()).read_to_end(&mut vec);
				vec
			}
			// Self::Lz4 => lz4_flex::compress(&data.to_be_file()),
			Self::Snbt => data.to_string().into_bytes(),
			format @ (Self::LittleEndianNbt | Self::LittleEndianHeaderNbt) => data.to_le_file(format == Self::LittleEndianHeaderNbt),
		}
	}

	#[must_use]
	pub const fn uv(self) -> Vec2u {
		match self {
			Self::Nbt => NBT_FILE_TYPE_UV,
			Self::Gzip => GZIP_FILE_TYPE_UV,
			Self::Zlib => ZLIB_FILE_TYPE_UV,
			Self::Snbt => SNBT_FILE_TYPE_UV,
			Self::Mca => MCA_FILE_TYPE_UV,
			Self::LittleEndianNbt => LITTLE_ENDIAN_NBT_FILE_TYPE_UV,
			Self::LittleEndianHeaderNbt => LITTLE_ENDIAN_HEADER_NBT_FILE_TYPE_UV,
			// Self::Lz4 => Vec2u::new(240, 240),
		}
	}

	#[must_use]
	pub const fn into_str(self) -> &'static str {
		match self {
			Self::Nbt => "Uncompressed",
			Self::Gzip => "GZip",
			Self::Zlib => "ZLib",
			Self::Snbt => "SNBT",
			Self::Mca => "MCA",
			// Self::Lz4 => "LZ4",
			Self::LittleEndianNbt => "Little Endian NBT",
			Self::LittleEndianHeaderNbt => "Little Endian NBT (With Header)",
		}
	}
}

impl Display for NbtFileFormat {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.into_str()) }
}

#[repr(u8)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum ChunkFileFormat {
	Gzip,
	#[default]
	Zlib,
	Nbt,
	Lz4,
}

impl ChunkFileFormat {
	#[must_use]
	pub const fn cycle(self) -> Self {
		match self {
			Self::Gzip => Self::Zlib,
			Self::Zlib => Self::Nbt,
			Self::Nbt => Self::Lz4,
			Self::Lz4 => Self::Gzip,
		}
	}

	#[must_use]
	pub const fn rev_cycle(self) -> Self {
		match self {
			Self::Gzip => Self::Lz4,
			Self::Zlib => Self::Gzip,
			Self::Nbt => Self::Zlib,
			Self::Lz4 => Self::Nbt,
		}
	}

	#[must_use]
	pub fn encode(self, data: &NbtElement) -> Vec<u8> {
		match self {
			Self::Nbt => data.to_be_file(),
			Self::Gzip => {
				let mut vec = vec![];
				let _ = flate2::read::GzEncoder::new(data.to_be_file().as_slice(), Compression::best()).read_to_end(&mut vec);
				vec
			}
			Self::Zlib => {
				let mut vec = vec![];
				let _ = flate2::read::ZlibEncoder::new(data.to_be_file().as_slice(), Compression::best()).read_to_end(&mut vec);
				vec
			}
			Self::Lz4 => lz4_flex::compress(&data.to_be_file()),
		}
	}

	#[must_use]
	pub const fn uv(self) -> Vec2u {
		match self {
			Self::Nbt => NBT_FILE_TYPE_UV,
			Self::Gzip => GZIP_FILE_TYPE_UV,
			Self::Zlib => ZLIB_FILE_TYPE_UV,
			// todo
			Self::Lz4 => Vec2u::new(240, 240),
		}
	}

	#[must_use]
	pub const fn into_str(self) -> &'static str {
		match self {
			Self::Nbt => "Uncompressed",
			Self::Gzip => "GZip",
			Self::Zlib => "ZLib",
			Self::Lz4 => "LZ4",
		}
	}
}

pub struct FilePath {
	path: PathBuf,
	cached_name: CompactString,
	cached_path_str: String,
}

impl FilePath {
	#[must_use]
	fn name_for_path(path: &Path) -> Option<CompactString> { path.file_name().map(|s| s.to_string_lossy().into_owned()).map(CompactString::from) }

	pub fn new(path: impl Into<PathBuf>) -> Result<Self, FilePathError> {
		let path = path.into();

		Ok(Self {
			cached_name: Self::name_for_path(&path).ok_or_else(|| FilePathError::PathHasNoName(path.clone()))?,
			cached_path_str: path.to_string_lossy().into_owned(),
			path,
		})
	}

	#[must_use]
	pub fn path(&self) -> &Path { &self.path }

	#[must_use]
	pub fn path_str(&self) -> &str { &self.cached_path_str }

	/// # Errors
	/// * If the path is not a valid file path.
	///
	/// # Returns
	/// `Ok(old_path)`\
	/// `Err(current_path)`
	pub fn set_path(&mut self, path: impl Into<PathBuf>) -> Result<PathBuf, FilePathError> {
		let path = path.into();
		self.cached_name = Self::name_for_path(&path).ok_or_else(|| FilePathError::PathHasNoName(path.clone()))?;
		self.cached_path_str = path.to_string_lossy().into_owned();
		Ok(core::mem::replace(&mut self.path, path))
	}

	#[must_use]
	pub fn name(&self) -> &str { &self.cached_name }
}

#[derive(Error, Debug)]
pub enum FilePathError {
	#[error("Path {0:?} has no name")]
	PathHasNoName(PathBuf),
}

impl AsRef<Path> for FilePath {
	fn as_ref(&self) -> &Path { &self.path }
}

#[derive(Copy, Clone)]
pub struct TabConstants {
	pub left_margin: usize,
	pub scroll: usize,
	pub horizontal_scroll: usize,
}
