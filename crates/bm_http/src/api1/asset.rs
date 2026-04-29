use std::{
	collections::BTreeSet,
	ffi::OsStr,
	hash::{Hash, Hasher},
	io::{Cursor, Write},
	time::Duration,
};

use aide::{
	axum::{ApiRouter, IntoApiResponse, routing::get_with},
	openapi,
	transform::TransformOperation,
};
use axum::{
	debug_handler,
	extract::{FromRef, OriginalUri, Request, State},
	http::{StatusCode, header},
	middleware,
	response::{IntoResponse, Response},
};
use axum_extra::{
	TypedHeader,
	headers::{CacheControl, ContentType, ETag, HeaderMapExt, IfNoneMatch},
};
use bm_asset::Format;
use schemars::{
	JsonSchema,
	r#gen::SchemaGenerator,
	schema::{InstanceType, Schema, SchemaObject},
};
use seahash::SeaHasher;
use serde::{Deserialize, Serialize};

use crate::service::Service;

use super::{
	api::ApiState,
	error::Result,
	extract::{Path, Query, VersionQuery},
	jsonschema::impl_jsonschema,
};

// NOTE: Bump this if changing any behavior that impacts output binary data for assets, to ensure ETag is cache-broken.
const ASSET_ETAG_VERSION: usize = 3;
const ICON_GROUP_COUNT: u32 = 1_000;
const ICONS_PER_GROUP: u32 = 1_000;
const MAP_INDEX_COUNT: u8 = 100;
const MAP_LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
const PERCHBIRD_PATHLIST_URL: &str = "https://rl2.perchbird.dev/download/PathList.gz";

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
	maxage: u64,
}

#[derive(Clone, FromRef)]
struct AssetState {
	services: Service,
	config: Config,
}

pub fn router(config: Config, state: ApiState) -> ApiRouter {
	let state = AssetState {
		services: state.services,
		config,
	};

	ApiRouter::new()
		.api_route("/", get_with(asset2, asset2_docs))
		.api_route("/icons", get_with(icons_archive, icons_archive_docs))
		.api_route("/icons/{group}", get_with(icon_archive, icon_archive_docs))
		.api_route("/maps", get_with(maps_archive, maps_archive_docs))
		.api_route("/map/{territory}/{index}", get_with(map, map_docs))
		.api_route("/uld/archive", get_with(uld_archive, uld_archive_docs))
		.api_route("/uld/export", get_with(uld_export, uld_export_docs))
		.api_route("/uld/references", get_with(uld_references, uld_references_docs))
		.api_route("/uld", get_with(uld, uld_docs))
		// Fall back to the old asset endpoint for compatibility.
		.route("/{*path}", axum::routing::get(asset1))
		.layer(middleware::from_fn_with_state(state.clone(), cache_layer))
		.with_state(state)
}

// Original asset endpoint based on a game path in the url path.

#[derive(Deserialize)]
struct Asset1Path {
	path: String,
}

#[derive(Deserialize)]
struct Asset1Query {
	format: SchemaFormat,
}

#[derive(Deserialize, JsonSchema)]
struct IconArchiveQuery {
	/// Format that extracted icons should be converted into.
	#[schemars(example = "example_format")]
	format: SchemaFormat,
}

#[derive(Deserialize, JsonSchema)]
struct MapQuery {
	/// Format that composed maps should be converted into.
	#[schemars(example = "example_map_format")]
	format: Option<SchemaFormat>,
}

#[derive(Deserialize, JsonSchema)]
struct UldQuery {
	/// Full ULD path to retrieve, for example `ui/uld/journal.uld`.
	#[schemars(example = "example_uld_path")]
	path: String,
}

#[derive(Deserialize, JsonSchema)]
struct UldArchiveQuery {
	/// Format that `.tex` files should be converted into inside the archive.
	#[schemars(example = "example_format")]
	format: SchemaFormat,
}

#[derive(Deserialize, JsonSchema)]
struct UldExportQuery {
	/// Full ULD path to parse and export.
	#[schemars(example = "example_uld_path")]
	path: String,

	/// Format that referenced `.tex` files should be converted into.
	#[schemars(example = "example_format")]
	format: SchemaFormat,
}

#[debug_handler(state = AssetState)]
async fn asset1(
	Path(Asset1Path { path }): Path<Asset1Path>,
	query_version: VersionQuery,
	Query(Asset1Query { format }): Query<Asset1Query>,
	state_service: State<Service>,
) -> Result<impl IntoApiResponse> {
	// The endpoints are nearly identical - just call through to the new endpoint with an emulated query.
	asset2(
		query_version,
		Query(AssetQuery { path, format }),
		state_service,
	)
	.await
}

/// Query parameters accepted by the asset endpoint.
#[derive(Deserialize, JsonSchema)]
struct AssetQuery {
	/// Game path of the asset to retrieve.
	#[schemars(example = "example_path")]
	path: String,

	/// Format that the asset should be converted into.
	#[schemars(example = "example_format")]
	format: SchemaFormat,
}

fn example_path() -> &'static str {
	"ui/icon/051000/051474_hr1.tex"
}

#[derive(Serialize, Deserialize)]
#[repr(transparent)]
struct SchemaFormat(Format);

impl_jsonschema!(SchemaFormat, format_schema);
fn format_schema(_generator: &mut SchemaGenerator) -> Schema {
	Schema::Object(SchemaObject {
		instance_type: Some(InstanceType::String.into()),
		enum_values: Some(
			Format::iter()
				.map(|format| serde_json::to_value(format).expect("should not fail"))
				.collect(),
		),
		..Default::default()
	})
}

fn example_format() -> SchemaFormat {
	SchemaFormat(Format::Png)
}

fn example_map_format() -> SchemaFormat {
	SchemaFormat(Format::Jpeg)
}

fn example_uld_path() -> &'static str {
	"ui/uld/journal.uld"
}

fn asset2_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("read an asset")
		.description("Read an asset from the game at the specified path, converting it into a usable format. If no valid conversion between the game file type and specified format exists, an error will be returned.")
		.response_with::<200, Vec<u8>, _>(|mut response| {
			response.inner().content = Format::iter()
				.map(|format| {
					(
						format_mime(format).to_string(),
						openapi::MediaType::default(),
					)
				})
				.collect();
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

#[debug_handler(state = AssetState)]
async fn asset2(
	VersionQuery(version_key): VersionQuery,
	Query(AssetQuery {
		path,
		format: SchemaFormat(format),
	}): Query<AssetQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	// Perform the conversion.
	// TODO: can this be made async?
	let bytes = asset.convert(version_key, &path, format)?;

	// Try to derive a filename to use for the Content-Disposition header.
	let filepath = std::path::Path::new(&path).with_extension(format.extension());
	let disposition = match filepath.file_name().and_then(OsStr::to_str) {
		Some(name) => format!("inline; filename=\"{name}\""),
		None => "inline".to_string(),
	};

	let response = (
		TypedHeader(ContentType::from(format_mime(format))),
		// TypedHeader only has a really naive inline value with no ability to customise :/
		[(header::CONTENT_DISPOSITION, disposition)],
		bytes,
	);

	Ok(response.into_response())
}

fn format_mime(format: Format) -> mime::Mime {
	match format {
		Format::Jpeg => mime::IMAGE_JPEG,
		Format::Png => mime::IMAGE_PNG,
		Format::Webp => "image/webp".parse().expect("mime parse should not fail"),
	}
}

/// Path segments expected by the asset map endpoint.
#[derive(Debug, Deserialize, JsonSchema)]
struct MapPath {
	/// Territory of the map to be retrieved. This typically takes the form of 4
	/// characters, [letter][number][letter][number]. See `Map`'s `Id` field for
	/// examples of possible combinations of `territory` and `index`.
	#[schemars(example = "example_territory")]
	territory: String,

	/// Index of the map within the territory. This invariably takes the form of a
	/// two-digit zero-padded number. See `Map`'s `Id` field for examples of
	/// possible combinations of `territory` and `index`.
	#[schemars(example = "example_index")]
	index: String,
}

fn example_territory() -> &'static str {
	"s1d1"
}

fn example_index() -> &'static str {
	"00"
}

fn map_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("compose a map")
		.description(
			"Retrieve the specified map, composing it from split source files if necessary.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			response.inner().content = Format::iter()
				.map(|format| {
					(
						format_mime(format).to_string(),
						openapi::MediaType::default(),
					)
				})
				.collect();
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

/// Path segments expected by the icon archive endpoint.
#[derive(Debug, Deserialize, JsonSchema)]
struct IconArchivePath {
	/// 6-digit icon group directory, such as `062000`.
	#[schemars(example = "example_icon_group")]
	group: String,
}

fn example_icon_group() -> &'static str {
	"062000"
}

fn icon_archive_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("download an icon archive")
		.description(
			"Build a zip archive containing every existing icon in the specified 1000-icon group, including `_hr1` variants when present.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert("application/zip".into(), openapi::MediaType::default());
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

fn icons_archive_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("download all icon archives")
		.description(
			"Build a zip archive containing every existing icon path that can be discovered from the standard 1000-icon groups, including `_hr1` variants when present.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert("application/zip".into(), openapi::MediaType::default());
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

fn maps_archive_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("download all maps")
		.description(
			"Build a zip archive containing every existing composed map image that can be discovered from standard territory and index combinations. Use the `format` query parameter to control the image format inside the archive.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert("application/zip".into(), openapi::MediaType::default());
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

fn uld_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("read a raw uld file")
		.description(
			"Retrieve a raw `.uld` file. This is the first step toward referenced-texture export and can be used to inspect UI layout data without asset conversion.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert(
				"application/octet-stream".into(),
				openapi::MediaType::default(),
			);
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

fn uld_archive_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("download a uld archive")
		.description(
			"Build a zip archive from the daily Perchbird path list, including raw `.uld` files and converted `ui/uld/*.tex` images.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert("application/zip".into(), openapi::MediaType::default());
			response
		})
		.response_with::<304, (), _>(|res| res.description("not modified"))
}

#[derive(Serialize, JsonSchema)]
struct UldReferencesResponse {
	path: String,
	image_nodes: Vec<bm_asset::ImageNodeReference>,
	texture_paths: Vec<String>,
}

fn uld_references_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("read parsed uld references")
		.description(
			"Parse a `.uld` file and return the discovered image nodes plus embedded texture paths referenced by the layout.",
		)
		.response_with::<200, axum::Json<UldReferencesResponse>, _>(|response| response)
}

fn uld_export_docs(operation: TransformOperation) -> TransformOperation {
	operation
		.summary("export a parsed uld layout")
		.description(
			"Parse a single `.uld` file and return a zip containing the raw layout plus only the referenced textures discovered inside it.",
		)
		.response_with::<200, Vec<u8>, _>(|mut response| {
			let content = &mut response.inner().content;
			content.clear();
			content.insert("application/zip".into(), openapi::MediaType::default());
			response
		})
}

#[debug_handler(state = AssetState)]
async fn icons_archive(
	VersionQuery(version_key): VersionQuery,
	Query(IconArchiveQuery { format }): Query<IconArchiveQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let mut writer = zip_writer();
	let extension = format.0.extension();
	let mut found = 0usize;

	for group_index in 0..ICON_GROUP_COUNT {
		let group = group_index * ICONS_PER_GROUP;
		found += write_icon_group(&mut writer, &asset, version_key, group, format.0)?;
	}

	if found == 0 {
		return Err(super::error::Error::NotFound(
			"no icons found in standard icon groups".into(),
		));
	}

	zip_response(
		writer,
		format!("ui_icons_all_{extension}.zip"),
	)
}

#[debug_handler(state = AssetState)]
async fn icon_archive(
	Path(IconArchivePath { group }): Path<IconArchivePath>,
	VersionQuery(version_key): VersionQuery,
	Query(IconArchiveQuery { format }): Query<IconArchiveQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let group = parse_icon_group(&group)?;
	let mut writer = zip_writer();
	let found = write_icon_group(&mut writer, &asset, version_key, group, format.0)?;

	if found == 0 {
		return Err(super::error::Error::NotFound(format!(
			"no icons found for group {group:06}"
		)));
	}

	zip_response(
		writer,
		format!("ui_icon_{group:06}_{}.zip", format.0.extension()),
	)
}

fn parse_icon_group(value: &str) -> Result<u32> {
	if value.len() != 6 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
		return Err(super::error::Error::Invalid(
			"icon group must be a 6-digit number like 062000".into(),
		));
	}

	let group = value
		.parse::<u32>()
		.map_err(|_| super::error::Error::Invalid("invalid icon group".into()))?;

	if group % 1000 != 0 {
		return Err(super::error::Error::Invalid(
			"icon group must end with 000".into(),
		));
	}

	Ok(group)
}

#[debug_handler(state = AssetState)]
async fn maps_archive(
	VersionQuery(version_key): VersionQuery,
	Query(MapQuery { format }): Query<MapQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let mut writer = zip_writer();
	let format = format.unwrap_or_else(default_map_format).0;
	let mut found = 0usize;

	for &a in MAP_LETTERS {
		for b in b'0'..=b'9' {
			for &c in MAP_LETTERS {
				for d in b'0'..=b'9' {
					let territory = format!(
						"{}{}{}{}",
						a as char, b as char, c as char, d as char
					);

					for index in 0..MAP_INDEX_COUNT {
						let index = format!("{index:02}");
						let bytes = match asset.map(version_key, &territory, &index, format) {
							Ok(bytes) => bytes,
							Err(bm_asset::Error::NotFound(..)) => continue,
							Err(error) => return Err(error.into()),
						};

						write_zip_file(
							&mut writer,
							&format!(
								"ui/map/{territory}/{index}/{territory}_{index}.{}",
								format.extension()
							),
							&bytes,
						)?;
						found += 1;
					}
				}
			}
		}
	}

	if found == 0 {
		return Err(super::error::Error::NotFound(
			"no maps found in standard territory/index combinations".into(),
		));
	}

	zip_response(
		writer,
		format!("ui_maps_all_{}.zip", format.extension()),
	)
}

#[debug_handler(state = AssetState)]
async fn uld(
	VersionQuery(version_key): VersionQuery,
	Query(UldQuery { path }): Query<UldQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let path = validate_uld_path(&path)?;
	let bytes = asset.raw(version_key, &path)?;
	let filename = std::path::Path::new(&path)
		.file_name()
		.and_then(OsStr::to_str)
		.unwrap_or("layout.uld");

	Ok((
		TypedHeader(
			ContentType::from(
				"application/octet-stream"
					.parse::<mime::Mime>()
					.expect("mime parse should not fail"),
			),
		),
		[(
			header::CONTENT_DISPOSITION,
			format!("attachment; filename=\"{filename}\""),
		)],
		bytes,
	)
		.into_response())
}

#[debug_handler(state = AssetState)]
async fn uld_references(
	VersionQuery(version_key): VersionQuery,
	Query(UldQuery { path }): Query<UldQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let path = validate_uld_path(&path)?;
	let parsed = asset.uld(version_key, &path)?;

	Ok(axum::Json(UldReferencesResponse {
		path,
		image_nodes: parsed.image_nodes,
		texture_paths: parsed.texture_paths,
	}))
}

#[debug_handler(state = AssetState)]
async fn uld_export(
	VersionQuery(version_key): VersionQuery,
	Query(UldExportQuery { path, format }): Query<UldExportQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let path = validate_uld_path(&path)?;
	let parsed = asset.uld(version_key, &path)?;
	let raw = asset.raw(version_key, &path)?;

	let mut writer = zip_writer();
	write_zip_file(&mut writer, &path, &raw)?;

	let mut found = 1usize;
	for texture_path in parsed.texture_paths {
		if !is_texture_path(&texture_path) {
			continue;
		}

		let bytes = match asset.convert(version_key, &texture_path, format.0) {
			Ok(bytes) => bytes,
			Err(bm_asset::Error::NotFound(..)) => continue,
			Err(error) => return Err(error.into()),
		};

		write_zip_file(
			&mut writer,
			&replace_extension(&texture_path, format.0.extension()),
			&bytes,
		)?;
		found += 1;
	}

	if found == 1 {
		return Err(super::error::Error::NotFound(format!(
			"no referenced textures found for {}",
			path
		)));
	}

	let stem = std::path::Path::new(&path)
		.file_stem()
		.and_then(OsStr::to_str)
		.unwrap_or("layout");
	zip_response(
		writer,
		format!("{}_uld_export_{}.zip", stem, format.0.extension()),
	)
}

#[debug_handler(state = AssetState)]
async fn uld_archive(
	VersionQuery(version_key): VersionQuery,
	Query(UldArchiveQuery { format }): Query<UldArchiveQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let paths = fetch_perchbird_paths("ui/uld/").await?;
	let mut writer = zip_writer();
	let mut found = 0usize;

	for path in paths {
		if path.to_ascii_lowercase().ends_with(".uld") {
			let bytes = match asset.raw(version_key, &path) {
				Ok(bytes) => bytes,
				Err(bm_asset::Error::NotFound(..)) => continue,
				Err(error) => return Err(error.into()),
			};

			write_zip_file(&mut writer, &path, &bytes)?;
			found += 1;
			continue;
		}

		if is_texture_path(&path) {
			let bytes = match asset.convert(version_key, &path, format.0) {
				Ok(bytes) => bytes,
				Err(bm_asset::Error::NotFound(..)) => continue,
				Err(error) => return Err(error.into()),
			};

			write_zip_file(
				&mut writer,
				&replace_extension(&path, format.0.extension()),
				&bytes,
			)?;
			found += 1;
		}
	}

	if found == 0 {
		return Err(super::error::Error::NotFound(
			"no uld files or textures found from path list".into(),
		));
	}

	zip_response(
		writer,
		format!("ui_uld_archive_{}.zip", format.0.extension()),
	)
}

#[debug_handler]
async fn map(
	Path(MapPath { territory, index }): Path<MapPath>,
	VersionQuery(version_key): VersionQuery,
	Query(MapQuery { format }): Query<MapQuery>,
	State(Service { asset, .. }): State<Service>,
) -> Result<impl IntoApiResponse> {
	let format = format.unwrap_or_else(default_map_format).0;
	let bytes = asset.map(version_key, &territory, &index, format)?;

	let response = (
		TypedHeader(ContentType::from(format_mime(format))),
		[(
			header::CONTENT_DISPOSITION,
			format!(
				"inline; filename=\"{territory}_{index}.{}\"",
				format.extension()
			),
		)],
		bytes,
	);

	Ok(response.into_response())
}

fn zip_writer() -> zip::ZipWriter<Cursor<Vec<u8>>> {
	zip::ZipWriter::new(Cursor::new(Vec::new()))
}

fn write_icon_group(
	writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>,
	asset: &bm_asset::Service,
	version_key: bm_version::VersionKey,
	group: u32,
	format: Format,
) -> Result<usize> {
	let extension = format.extension();
	let mut found = 0usize;

	for id in group..(group + ICONS_PER_GROUP) {
		for suffix in ["", "_hr1"] {
			let source_path = format!("ui/icon/{group:06}/{id:06}{suffix}.tex");

			let bytes = match asset.convert(version_key, &source_path, format) {
				Ok(bytes) => bytes,
				Err(bm_asset::Error::NotFound(..)) => continue,
				Err(error) => return Err(error.into()),
			};

			write_zip_file(
				writer,
				&format!("ui/icon/{group:06}/{id:06}{suffix}.{extension}"),
				&bytes,
			)?;
			found += 1;
		}
	}

	Ok(found)
}

fn write_zip_file(
	writer: &mut zip::ZipWriter<Cursor<Vec<u8>>>,
	path: &str,
	bytes: &[u8],
) -> Result<()> {
	let options = zip::write::SimpleFileOptions::default()
		.compression_method(zip::CompressionMethod::Deflated);
	writer
		.start_file(path, options)
		.map_err(anyhow::Error::from)?;
	writer.write_all(bytes).map_err(anyhow::Error::from)?;
	Ok(())
}

fn zip_response(
	writer: zip::ZipWriter<Cursor<Vec<u8>>>,
	filename: String,
) -> Result<Response> {
	let bytes = writer.finish().map_err(anyhow::Error::from)?.into_inner();

	Ok((
		TypedHeader(
			ContentType::from(
				"application/zip"
					.parse::<mime::Mime>()
					.expect("mime parse should not fail"),
			),
		),
		[(
			header::CONTENT_DISPOSITION,
			format!("attachment; filename=\"{filename}\""),
		)],
		bytes,
	)
		.into_response())
}

fn default_map_format() -> SchemaFormat {
	SchemaFormat(Format::Jpeg)
}

fn validate_uld_path(path: &str) -> Result<String> {
	let normalized = path.trim().replace('\\', "/");

	if !normalized.starts_with("ui/uld/") {
		return Err(super::error::Error::Invalid(
			"uld path must start with ui/uld/".into(),
		));
	}

	if !normalized.ends_with(".uld") {
		return Err(super::error::Error::Invalid(
			"uld path must end with .uld".into(),
		));
	}

	Ok(normalized)
}

async fn fetch_perchbird_paths(prefix: &str) -> Result<Vec<String>> {
	let response = reqwest::get(PERCHBIRD_PATHLIST_URL)
		.await
		.map_err(anyhow::Error::from)?;
	let bytes = response.bytes().await.map_err(anyhow::Error::from)?;

	let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(bytes));
	let reader = std::io::BufReader::new(decoder);
	let mut unique = BTreeSet::new();

	for line in std::io::BufRead::lines(reader) {
		let line = line.map_err(anyhow::Error::from)?;
		let path = line.trim();
		if path.starts_with(prefix) {
			unique.insert(path.to_string());
		}
	}

	Ok(unique.into_iter().collect())
}

fn replace_extension(path: &str, extension: &str) -> String {
	let stem = strip_texture_extension(path).unwrap_or(path);
	format!("{stem}.{extension}")
}

fn strip_texture_extension(path: &str) -> Option<&str> {
	for suffix in [".tex", ".atex"] {
		if let Some(stem) = path.strip_suffix(suffix) {
			return Some(stem);
		}
	}

	None
}

fn is_texture_path(path: &str) -> bool {
	let lower = path.to_ascii_lowercase();
	lower.ends_with(".tex") || lower.ends_with(".atex")
}

async fn cache_layer(
	uri: OriginalUri,
	VersionQuery(version): VersionQuery,
	header_if_none_match: Option<TypedHeader<IfNoneMatch>>,
	State(config): State<Config>,
	request: Request,
	next: middleware::Next,
) -> Response {
	// Build ETag for this request.
	let mut hasher = SeaHasher::new();
	uri.hash(&mut hasher);
	let uri_hash = hasher.finish();

	let etag = format!("\"{uri_hash:016x}.{version}.{ASSET_ETAG_VERSION}\"")
		.parse::<ETag>()
		.expect("malformed etag");

	// If the request came through with a passing ETag, we can skip doing any processing.
	if let Some(TypedHeader(if_none_match)) = header_if_none_match {
		if !if_none_match.precondition_passes(&etag) {
			return StatusCode::NOT_MODIFIED.into_response();
		}
	}

	// ETag didn't match, pass down to the rest of the handlers.
	let mut response = next.run(request).await;

	// Add cache headers.
	let cache_control = CacheControl::new()
		.with_public()
		.with_immutable()
		.with_max_age(Duration::from_secs(config.maxage));

	let headers = response.headers_mut();
	headers.typed_insert(etag);
	headers.typed_insert(cache_control);

	response
}
