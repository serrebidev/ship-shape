use patois::t;

use crate::{
	UpdateError, UpdaterConfig,
	github::{self, AssetPair, GithubRelease},
	version::{Version, parse_semver},
};

const SHORT_HASH_LEN: usize = 7;

/// Which release stream to check against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdateChannel {
	/// Compare semver tags against the latest GitHub release.
	#[default]
	Stable,
	/// Compare short commit hashes against a rolling `"latest"` pre-release tag.
	Dev,
}

/// A newer release that can be downloaded with [`download_update_file`](crate::download_update_file).
#[derive(Debug)]
pub struct UpdateAvailableResult {
	/// Release tag on the stable channel, or `dev-{short hash}` on the dev channel.
	pub latest_version: String,
	/// URL of the release asset for this platform and install kind.
	pub download_url: String,
	/// URL of the minisign signature for `download_url`.
	pub signature_url: String,
	/// Markdown release notes. On the dev channel, only the commits newer than the running build.
	pub release_notes: String,
}

/// Result of a successful update check.
#[derive(Debug)]
pub enum UpdateCheckOutcome {
	/// A newer release exists.
	UpdateAvailable(UpdateAvailableResult),
	/// The running app is current. Holds the latest version found.
	UpToDate(String),
}

/// Check whether an update is available for the app described by `config`.
///
/// [`UpdateChannel::Stable`] compares [`UpdaterConfig::current_version`] against the latest
/// release tag. [`UpdateChannel::Dev`] compares [`UpdaterConfig::current_commit`] against the
/// rolling `"latest"` release.
///
/// # Errors
///
/// Returns [`UpdateError`] on network failure, HTTP error, invalid version strings, or missing
/// release assets.
pub fn check_for_updates(config: &UpdaterConfig, channel: UpdateChannel) -> Result<UpdateCheckOutcome, UpdateError> {
	match channel {
		UpdateChannel::Stable => check_stable(config),
		UpdateChannel::Dev => check_dev(config),
	}
}

fn check_stable(config: &UpdaterConfig) -> Result<UpdateCheckOutcome, UpdateError> {
	let current = parse_semver(&config.current_version)
		.ok_or_else(|| UpdateError::InvalidVersion(t("Current version is not a valid semver.")))?;
	let release = github::fetch_latest_release(config)?;
	let latest = parse_semver(&release.tag_name)
		.ok_or_else(|| UpdateError::InvalidResponse(t("Latest release tag is not a valid semver.")))?;
	if current >= latest {
		return Ok(UpdateCheckOutcome::UpToDate(release.tag_name));
	}
	// An update spanning several releases shows every skipped
	// version's notes, newest first — someone upgrading across
	// versions wants the fixes from all of them, not just the
	// newest. The releases list is best-effort: if it fails,
	// fall back to the latest release's notes alone.
	let notes = match github::fetch_releases(config) {
		Ok(releases) => combine_notes(&releases, current, latest),
		Err(_) => release.body.clone().unwrap_or_default(),
	};
	update_available(config, &release, release.tag_name.clone(), notes)
}

/// Release notes for every version in (`current`, `latest`],
/// newest first, each under its own "## <tag>" heading — the
/// same shape the View Changelog window renders. Releases whose
/// tags don't parse as versions are skipped, as are drafts;
/// a release with no body contributes its heading only.
fn combine_notes(releases: &[GithubRelease], current: Version, latest: Version) -> String {
	let mut picked: Vec<(Version, &GithubRelease)> = releases
		.iter()
		.filter(|r| !r.draft)
		.filter_map(|r| parse_semver(&r.tag_name).map(|v| (v, r)))
		.filter(|(v, _)| *v > current && *v <= latest)
		.collect();
	// The API returns newest first already; sort anyway so the
	// order never depends on the host's listing order.
	picked.sort_by_key(|(v, _)| std::cmp::Reverse(*v));
	let mut out = String::new();
	for (_, release) in picked {
		if !out.is_empty() {
			out.push_str("\n\n");
		}
		out.push_str("## ");
		out.push_str(&release.tag_name);
		let body = release.body.as_deref().unwrap_or_default().trim();
		if !body.is_empty() {
			out.push_str("\n\n");
			out.push_str(body);
		}
	}
	out
}

fn check_dev(config: &UpdaterConfig) -> Result<UpdateCheckOutcome, UpdateError> {
	let release = github::fetch_release_by_tag(config, "latest")?;
	let raw_notes = release.body.clone().unwrap_or_default();
	let commit_lines: Vec<&str> = raw_notes.lines().filter(|l| l.trim().starts_with("- ")).collect();
	let current_commit = config.current_commit.as_str();
	let short_current = current_commit.get(..SHORT_HASH_LEN).unwrap_or(current_commit);
	if commit_lines.is_empty() {
		return Ok(UpdateCheckOutcome::UpToDate(format!("dev-{short_current}")));
	}
	let latest_hash = commit_lines.first().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("latest");
	let latest_version = format!("dev-{latest_hash}");
	if short_current == latest_hash {
		return Ok(UpdateCheckOutcome::UpToDate(latest_version));
	}
	let position =
		if short_current.is_empty() { None } else { commit_lines.iter().position(|l| l.contains(short_current)) };
	match position {
		Some(0) => Ok(UpdateCheckOutcome::UpToDate(latest_version)),
		Some(pos) => {
			let new_notes = commit_lines[..pos].join("\n");
			update_available(config, &release, latest_version, new_notes)
		}
		// Commit not found in recent history; assume it's old and offer full update.
		None => update_available(config, &release, latest_version, raw_notes),
	}
}

fn update_available(
	config: &UpdaterConfig,
	release: &GithubRelease,
	latest_version: String,
	release_notes: String,
) -> Result<UpdateCheckOutcome, UpdateError> {
	let AssetPair { download_url, signature_url } = github::require_asset_pair(config, release)?;
	Ok(UpdateCheckOutcome::UpdateAvailable(UpdateAvailableResult {
		latest_version,
		download_url,
		signature_url,
		release_notes,
	}))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn rel(tag: &str, body: Option<&str>) -> GithubRelease {
		GithubRelease { tag_name: tag.into(), body: body.map(str::to_string), draft: false, assets: Vec::new() }
	}

	fn v(major: u32, minor: u32, patch: u32) -> Version {
		parse_semver(&format!("{major}.{minor}.{patch}")).expect("test version parses")
	}

	#[test]
	fn combine_notes_covers_every_skipped_version_newest_first() {
		// Brandon's case: running 0.1.85, latest is 0.1.88 — the
		// notes must include .86 and .87, not just .88.
		let releases = vec![
			rel("v0.1.88", Some("- fix A")),
			rel("v0.1.87", Some("- fix B")),
			rel("v0.1.86", Some("- fix C")),
			rel("v0.1.85", Some("- old")),
		];
		let out = combine_notes(&releases, v(0, 1, 85), v(0, 1, 88));
		assert!(out.starts_with("## v0.1.88\n\n- fix A"), "{out}");
		assert!(out.contains("## v0.1.87\n\n- fix B"), "{out}");
		assert!(out.contains("## v0.1.86\n\n- fix C"), "{out}");
		assert!(!out.contains("- old"), "the running version's own notes are excluded: {out}");
		assert!(out.find("v0.1.88").unwrap() < out.find("v0.1.87").unwrap());
		assert!(out.find("v0.1.87").unwrap() < out.find("v0.1.86").unwrap());
	}

	#[test]
	fn combine_notes_single_step_is_just_that_version() {
		let releases = vec![rel("v0.1.86", Some("- fix C")), rel("v0.1.85", Some("- old"))];
		let out = combine_notes(&releases, v(0, 1, 85), v(0, 1, 86));
		assert_eq!(out, "## v0.1.86\n\n- fix C");
	}

	#[test]
	fn combine_notes_skips_drafts_unversioned_tags_and_empty_bodies() {
		let mut draft = rel("v0.1.87", Some("- secret"));
		draft.draft = true;
		let releases = vec![rel("v0.1.88", None), draft, rel("latest", Some("- dev")), rel("v0.1.86", Some(""))];
		let out = combine_notes(&releases, v(0, 1, 85), v(0, 1, 88));
		assert_eq!(out, "## v0.1.88\n\n## v0.1.86");
	}
}
