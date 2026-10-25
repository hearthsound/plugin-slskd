//! The remote files of one download: how they are grouped into batches, and
//! where they land under the import root.
//!
//! slskd's batch API takes the files of one batch and downloads them into one
//! destination directory. Whether it keeps a file's remote subfolder under that
//! directory or flattens it to its file name is not documented by the instance
//! the plugin talks to, so a batch still holds the files of a single remote
//! directory and the plugin accepts either layout: the path of a downloaded file
//! is its destination plus either its name or its path relative to the batch's
//! directory, and the completion check looks for both. That is what makes a
//! download verifiable: the plugin knows where each file can appear, and the
//! server can hand the file on without asking slskd anything.
//!
//! Everything here is pure: it never touches the network, so it is unit-tested
//! on the host target.

use hearthsound_plugin::types::ProviderError;

use crate::slskd;
use crate::tokens;

/// Directory under slskd's downloads directory that this plugin owns.
///
/// Every file the plugin downloads lands under this directory, so an
/// administrator can point the import root at slskd's downloads directory and
/// the plugin's files stay apart from anything else slskd downloaded.
pub const PREFIX: &str = "hearthsound";

/// Returns the batch destination of one download.
///
/// # Parameters
///
/// - `key`: the download's random key.
///
/// # Returns
///
/// The destination, relative to slskd's downloads directory.
///
/// # Example
///
/// ```text
/// destination("0f8b") -> "hearthsound/0f8b"
/// ```
pub fn destination(key: &str) -> String {
    format!("{PREFIX}/{key}")
}

/// Returns the path one downloaded file has under the import root.
///
/// # Parameters
///
/// - `key`: the download's random key.
/// - `name`: the file's name or the file's path relative to its batch's
///   directory, which is what [`tokens::ExpectedFile`] carries as `name` and as
///   `relative`.
///
/// # Returns
///
/// The path, relative to the import root.
///
/// # Example
///
/// ```text
/// pinned_path("0f8b", "01 Song.flac") -> "hearthsound/0f8b/01 Song.flac"
/// pinned_path("0f8b", "CD1/01 Song.flac") -> "hearthsound/0f8b/CD1/01 Song.flac"
/// ```
pub fn pinned_path(key: &str, name: &str) -> String {
    format!("{}/{}", destination(key), name)
}

/// A set of remote files that share one directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// Remote directory the files share, with the peer's own separators.
    pub directory: String,
    /// The files of the group, in the order they were selected.
    pub files: Vec<tokens::RemoteFile>,
}

/// Selects the files a download asks for.
///
/// # Parameters
///
/// - `files`: the remote files the candidate offers.
/// - `selected`: the indices the server asked for, in the order it asked for
///   them; an empty list means every file of the candidate.
///
/// # Returns
///
/// The selected files.
///
/// # Errors
///
/// Returns [`ProviderError::Internal`] when an index does not name a file of
/// the candidate, which means the server and the plugin disagree about the
/// candidate's files.
pub fn select(
    files: &[tokens::RemoteFile],
    selected: &[u32],
) -> Result<Vec<tokens::RemoteFile>, ProviderError> {
    if selected.is_empty() {
        return Ok(files.to_vec());
    }

    selected
        .iter()
        .map(|index| {
            files.get(*index as usize).cloned().ok_or_else(|| {
                ProviderError::Internal(format!(
                    "the candidate has no file {index}; it offers {}",
                    files.len()
                ))
            })
        })
        .collect()
}

/// Groups remote files by the directory they are served from.
///
/// # Parameters
///
/// - `files`: the selected remote files.
///
/// # Returns
///
/// One group per distinct remote directory, in first-seen order, each holding
/// its files in the order they were selected.
///
/// # Example
///
/// ```text
/// group_by_directory([A\one.flac, A\two.flac, B\one.mp3])
///     -> [Group { directory: "A", files: [one.flac, two.flac] },
///         Group { directory: "B", files: [one.mp3] }]
/// ```
pub fn group_by_directory(files: &[tokens::RemoteFile]) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();

    for file in files {
        let directory = slskd::directory(&file.filename);

        match groups.iter_mut().find(|group| group.directory == directory) {
            Some(group) => group.files.push(file.clone()),
            None => groups.push(Group {
                directory: directory.to_string(),
                files: vec![file.clone()],
            }),
        }
    }

    groups
}

/// Returns the files a batch is expected to produce.
///
/// # Parameters
///
/// - `directory`: the remote directory the batch's files are served from, as
///   [`Group::directory`] names it.
/// - `files`: the remote files of one batch.
///
/// # Returns
///
/// One entry per file, holding its remote path, the name it lands under when
/// slskd flattens the batch, the path it lands under when slskd keeps the
/// remote subfolder, and its size.
pub fn expected(directory: &str, files: &[tokens::RemoteFile]) -> Vec<tokens::ExpectedFile> {
    files
        .iter()
        .map(|file| tokens::ExpectedFile {
            filename: file.filename.clone(),
            name: slskd::basename(&file.filename).to_string(),
            relative: relative(directory, &file.filename),
            size: file.size,
        })
        .collect()
}

/// Returns a remote path relative to the directory it is served from.
///
/// # Parameters
///
/// - `directory`: the remote directory, with the peer's own separators.
/// - `filename`: the remote path of the file.
///
/// # Returns
///
/// Everything after the directory and its separator, with the peer's
/// separators turned into `/`, which is what a path under an import root uses.
/// A path that does not start with the directory is returned whole, so the
/// completion check still looks at the file's own name.
///
/// # Example
///
/// ```text
/// relative("@@tqkz\\Music\\Album", "@@tqkz\\Music\\Album\\CD1\\01 Song.flac")
///     -> "CD1/01 Song.flac"
/// relative("@@tqkz\\Music\\Album", "@@tqkz\\Music\\Album\\01 Song.flac")
///     -> "01 Song.flac"
/// ```
fn relative(directory: &str, filename: &str) -> String {
    let rest = filename.strip_prefix(directory).unwrap_or(filename);
    let rest = rest.trim_start_matches(['\\', '/']);

    if rest.is_empty() {
        return slskd::basename(filename).to_string();
    }

    rest.replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a remote file with the given path and size.
    fn file(filename: &str, size: u64) -> tokens::RemoteFile {
        tokens::RemoteFile {
            filename: filename.to_string(),
            size,
        }
    }

    #[test]
    fn builds_the_destination_and_the_pinned_path() {
        assert_eq!(destination("0f8b"), "hearthsound/0f8b");
        assert_eq!(
            pinned_path("0f8b", "01 Song.flac"),
            "hearthsound/0f8b/01 Song.flac"
        );
    }

    #[test]
    fn selects_the_asked_for_files() {
        let files = vec![file("A\\one.flac", 1), file("A\\two.flac", 2)];

        assert_eq!(select(&files, &[1]).unwrap(), vec![file("A\\two.flac", 2)]);
        assert_eq!(select(&files, &[]).unwrap(), files);
    }

    #[test]
    fn rejects_an_index_the_candidate_does_not_have() {
        let files = vec![file("A\\one.flac", 1)];
        let error = select(&files, &[1]).unwrap_err();

        assert!(matches!(error, ProviderError::Internal(message) if message.contains("no file 1")));
    }

    #[test]
    fn groups_files_by_their_remote_directory() {
        let files = vec![
            file("@@tqkz\\Music\\Album\\01 Song.flac", 1),
            file("@@tqkz\\Music\\Other\\02 Song.flac", 2),
            file("@@tqkz\\Music\\Album\\03 Song.flac", 3),
        ];

        let groups = group_by_directory(&files);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].directory, "@@tqkz\\Music\\Album");
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[1].directory, "@@tqkz\\Music\\Other");
        assert_eq!(
            groups[1].files,
            vec![file("@@tqkz\\Music\\Other\\02 Song.flac", 2)]
        );
    }

    #[test]
    fn groups_a_file_without_a_directory_on_its_own() {
        let groups = group_by_directory(&[file("one.flac", 1), file("two.flac", 2)]);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].directory, "");
        assert_eq!(groups[0].files.len(), 2);
    }

    #[test]
    fn expects_the_basename_and_size_of_every_file() {
        let expected = expected(
            "@@tqkz\\Music\\Album",
            &[file("@@tqkz\\Music\\Album\\01 Song.flac", 4_841)],
        );

        assert_eq!(
            expected,
            vec![tokens::ExpectedFile {
                filename: "@@tqkz\\Music\\Album\\01 Song.flac".to_string(),
                name: "01 Song.flac".to_string(),
                relative: "01 Song.flac".to_string(),
                size: 4_841,
            }]
        );
    }

    #[test]
    fn expects_the_path_of_a_file_in_a_remote_subfolder() {
        let expected = expected(
            "@@tqkz\\Music\\Album",
            &[file("@@tqkz\\Music\\Album\\CD1\\01 - x.flac", 4_841)],
        );

        assert_eq!(
            expected,
            vec![tokens::ExpectedFile {
                filename: "@@tqkz\\Music\\Album\\CD1\\01 - x.flac".to_string(),
                name: "01 - x.flac".to_string(),
                relative: "CD1/01 - x.flac".to_string(),
                size: 4_841,
            }]
        );
    }

    #[test]
    fn keeps_a_path_that_a_group_directory_does_not_prefix() {
        assert_eq!(relative("A", "B\\one.flac"), "B/one.flac");
        assert_eq!(relative("", "one.flac"), "one.flac");
        assert_eq!(relative("A", "A"), "A");
    }
}
