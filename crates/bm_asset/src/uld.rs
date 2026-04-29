use std::collections::BTreeSet;
use std::io::{Cursor, SeekFrom};

use anyhow::Context;
use binrw::{BinRead, binrw};
use schemars::JsonSchema;
use serde::Serialize;

use super::error::Result;

#[binrw]
#[brw(repr = i32)]
#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize)]
pub enum NodeType {
	Unk1 = 0x1,
	Image = 0x2,
}

#[binrw]
#[br(import(node_type: NodeType))]
#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeData {
	#[br(pre_assert(node_type == NodeType::Image))]
	Image {
		part_list_id: u32,
		part_id: u32,
		#[br(map = read_bool_from::<u8>)]
		#[bw(map = write_bool_as::<u8>)]
		flip_horizontal: bool,
		#[br(map = read_bool_from::<u8>)]
		#[bw(map = write_bool_as::<u8>)]
		flip_vertical: bool,
		wrap: u8,
		unk1: u8,
	},
	#[serde(other)]
	Unknown,
}

#[binrw]
#[derive(Debug, Serialize)]
#[brw(little)]
pub struct WidgetNode {
	pub node_id: u32,
	pub parent_id: i32,
	next_sibling_id: i32,
	previous_sibling_id: i32,
	child_node_id: i32,
	node_type: NodeType,
	node_offset: u16,
	tab_index: i16,
	unk1: [i32; 4],
	pub x: i16,
	pub y: i16,
	pub width: u16,
	pub height: u16,
	rotation: f32,
	pub scale_x: f32,
	pub scale_y: f32,
	pub origin_x: i16,
	pub origin_y: i16,
	priority: u16,
	unk2: u8,
	unk3: u8,
	pub multiply_red: i16,
	pub multiply_green: i16,
	pub multiply_blue: i16,
	pub add_red: i16,
	pub add_green: i16,
	pub add_blue: i16,
	pub alpha: u8,
	clip_count: u8,
	pub timeline_id: u16,
	#[br(args(node_type))]
	pub data: NodeData,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct WidgetHeader {
	common: CommonHeader,
	unk1: u32,
	unk2: i32,
	pub id: u32,
	alignment_type: u8,
	supports_theming: u8,
	padding: [u8; 2],
	pub x: i16,
	pub y: i16,
	node_count: u16,
	offset: u16,
	#[br(count = node_count)]
	pub nodes: Vec<WidgetNode>,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct TimelineHeader {
	common: CommonHeader,
	timeline_count: u32,
	unk2: i32,
	#[br(count = timeline_count)]
	pub timelines: Vec<Timeline>,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct Timeline {
	id: u32,
	offset: u32,
	num_frames_1: u16,
	num_frames_2: u16,
	#[br(count = num_frames_1 + num_frames_2)]
	pub frames: Vec<TimelineFrame>,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct TimelineFrame {
	pub start_frame: u32,
	pub end_frame: u32,
	offset: u32,
	keygroup_count: u32,
	#[br(count = keygroup_count)]
	keygroups: Vec<TimelineKeyGroup>,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
struct TimelineKeyGroup {
	usage: u16,
	key_group_type: u16,
	offset: u16,
	keyframe_count: u16,
	#[br(count = 0)]
	keyframes: Vec<TimelineKeyFrame>,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
struct TimelineKeyFrame {
	time: u32,
	offset: u16,
	interpolation: u8,
	unk1: u8,
	acceleration: f32,
	decelration: f32,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct AtkHeader {
	common: CommonHeader,
	pub asset_list_offset: u32,
	pub part_list_offset: u32,
	component_list_offset: u32,
	timeline_list_offset: u32,
	widget_offset: u32,
	rewrite_data_offset: u32,
	timeline_count: u32,
	#[br(if(timeline_list_offset > 0))]
	#[br(restore_position, seek_before = SeekFrom::Current(timeline_list_offset as i64 - ATK_HEADER_SIZE as i64))]
	pub timeline: Option<TimelineHeader>,
	#[br(if(widget_offset > 0))]
	#[br(restore_position, seek_before = SeekFrom::Current(widget_offset as i64 - ATK_HEADER_SIZE as i64))]
	pub widget: Option<WidgetHeader>,
}

const ATK_HEADER_SIZE: usize = 36;

#[binrw]
#[derive(Debug)]
#[brw(little)]
struct CommonHeader {
	#[br(count = 4)]
	#[br(map = read_string)]
	#[bw(map = write_string)]
	identifier: String,
	#[br(count = 4)]
	#[br(map = read_string)]
	#[bw(map = write_string)]
	version: String,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
struct UldHeader {
	common: CommonHeader,
	component_offset: u32,
	widget_offset: u32,
}

#[binrw]
#[derive(Debug)]
#[brw(little)]
pub struct Uld {
	header: UldHeader,
	#[br(restore_position)]
	#[br(seek_before = SeekFrom::Start(header.component_offset as u64))]
	pub component: AtkHeader,
	#[br(restore_position)]
	#[br(seek_before = SeekFrom::Start(header.widget_offset as u64))]
	pub widget: AtkHeader,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ImageNodeReference {
	pub node_id: u32,
	pub part_list_id: u32,
	pub part_id: u32,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ParsedUld {
	pub image_nodes: Vec<ImageNodeReference>,
	pub texture_paths: Vec<String>,
}

impl ParsedUld {
	pub fn parse(bytes: &[u8]) -> Result<Self> {
		let mut cursor = Cursor::new(bytes);
		let uld = Uld::read(&mut cursor).context("failed to parse uld structure")?;

		let mut image_nodes = Vec::new();
		collect_image_nodes(&mut image_nodes, uld.component.widget.as_ref());
		collect_image_nodes(&mut image_nodes, uld.widget.widget.as_ref());

		Ok(Self {
			image_nodes,
			texture_paths: extract_texture_paths(bytes),
		})
	}
}

fn collect_image_nodes(output: &mut Vec<ImageNodeReference>, widget: Option<&WidgetHeader>) {
	let Some(widget) = widget else {
		return;
	};

	for node in &widget.nodes {
		if let NodeData::Image {
			part_list_id,
			part_id,
			..
		} = node.data
		{
			output.push(ImageNodeReference {
				node_id: node.node_id,
				part_list_id,
				part_id,
			});
		}
	}
}

fn extract_texture_paths(bytes: &[u8]) -> Vec<String> {
	let mut current = Vec::new();
	let mut paths = BTreeSet::new();

	for &byte in bytes {
		if is_path_byte(byte) {
			current.push(byte);
			continue;
		}

		maybe_store_path(&mut paths, &mut current);
	}

	maybe_store_path(&mut paths, &mut current);

	paths.into_iter().collect()
}

fn maybe_store_path(paths: &mut BTreeSet<String>, current: &mut Vec<u8>) {
	if current.len() < 8 {
		current.clear();
		return;
	}

	let candidate = String::from_utf8_lossy(current).to_lowercase();
	current.clear();

	if candidate.starts_with("ui/") && (candidate.ends_with(".tex") || candidate.ends_with(".atex"))
	{
		paths.insert(candidate);
	}
}

fn is_path_byte(byte: u8) -> bool {
	matches!(
		byte,
		b'a'..=b'z'
			| b'A'..=b'Z'
			| b'0'..=b'9'
			| b'/'
			| b'_'
			| b'.'
			| b'-'
	)
}

fn read_bool_from<T>(value: T) -> bool
where
	T: Into<u64>,
{
	value.into() != 0
}

fn write_bool_as<T>(value: &bool) -> T
where
	T: From<u8>,
{
	T::from(u8::from(*value))
}

fn read_string(bytes: Vec<u8>) -> String {
	String::from_utf8_lossy(&bytes)
		.trim_end_matches('\0')
		.to_string()
}

fn write_string(value: &String) -> Vec<u8> {
	value.as_bytes().to_vec()
}
