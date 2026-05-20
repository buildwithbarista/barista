//! ByteStream resource-name grammar parsing for the REAPI surface.
//!
//! The REAPI overlays a path-like grammar on the otherwise opaque
//! `google.bytestream` `resource_name` field. Roastery parses the two
//! shapes it serves:
//!
//! - **Read** — `{instance_name}/blobs/{hash}/{size}`
//! - **Write** — `{instance_name}/uploads/{uuid}/blobs/{hash}/{size}[/...]`
//!
//! `{instance_name}` is allowed to be empty and may itself contain
//! slashes, so the parser does not split on `/` blindly. Instead it
//! locates the `blobs` / `uploads` keyword segment and reads the fixed
//! tail that follows it. Anything after the `{size}` on a write
//! resource name (optional client metadata) is ignored, per the spec.
//!
//! v0.1 supports only the uncompressed (`IDENTITY`) form. The
//! `compressed-blobs/{compressor}/...` variant is rejected with
//! `UNIMPLEMENTED` — roastery stores raw bytes and does not transcode.

use tonic::Status;

use crate::storage::Digest;

/// A parsed `Read` resource name: `{instance}/blobs/{hash}/{size}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadResource {
    /// The (possibly empty) instance name preceding `blobs`.
    pub instance: String,
    /// The blob digest.
    pub digest: Digest,
    /// The declared size in bytes.
    pub size: u64,
}

/// A parsed `Write` resource name:
/// `{instance}/uploads/{uuid}/blobs/{hash}/{size}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResource {
    /// The (possibly empty) instance name preceding `uploads`.
    pub instance: String,
    /// The client-supplied upload UUID (opaque to the server).
    pub uuid: String,
    /// The blob digest.
    pub digest: Digest,
    /// The declared size in bytes.
    pub size: u64,
}

/// Parse a `Read` resource name. Returns a gRPC `INVALID_ARGUMENT`
/// status describing the malformation on any parse failure, or
/// `UNIMPLEMENTED` for the compressed-blobs variant roastery doesn't
/// serve in v0.1.
pub fn parse_read_resource(name: &str) -> Result<ReadResource, Status> {
    let segments: Vec<&str> = name.split('/').collect();

    // Reject the compressed variant explicitly so the client gets an
    // honest "not supported" rather than a confusing parse error.
    if segments.contains(&"compressed-blobs") {
        return Err(Status::unimplemented(
            "compressed-blobs resource names are not supported (v0.1 stores raw bytes)",
        ));
    }

    // Find the `blobs` keyword. Everything before it is the instance
    // name; the two segments after it are {hash}/{size}.
    let idx = segments.iter().position(|s| *s == "blobs").ok_or_else(|| {
        Status::invalid_argument(format!(
            "read resource name {name:?} does not contain a 'blobs' segment"
        ))
    })?;

    let hash = segments.get(idx + 1).ok_or_else(|| {
        Status::invalid_argument(format!("read resource name {name:?} is missing the hash"))
    })?;
    let size_str = segments.get(idx + 2).ok_or_else(|| {
        Status::invalid_argument(format!("read resource name {name:?} is missing the size"))
    })?;

    let digest = Digest::from_hex(hash)
        .map_err(|e| Status::invalid_argument(format!("invalid hash in resource name: {e}")))?;
    let size = parse_size(size_str, name)?;
    let instance = segments[..idx].join("/");

    Ok(ReadResource {
        instance,
        digest,
        size,
    })
}

/// Parse a `Write` resource name. Same error contract as
/// [`parse_read_resource`].
pub fn parse_write_resource(name: &str) -> Result<WriteResource, Status> {
    let segments: Vec<&str> = name.split('/').collect();

    if segments.contains(&"compressed-blobs") {
        return Err(Status::unimplemented(
            "compressed-blobs resource names are not supported (v0.1 stores raw bytes)",
        ));
    }

    // Locate `uploads`; the segment after it is the UUID, then `blobs`,
    // then {hash}/{size}.
    let up = segments.iter().position(|s| *s == "uploads").ok_or_else(|| {
        Status::invalid_argument(format!(
            "write resource name {name:?} does not contain an 'uploads' segment"
        ))
    })?;

    let uuid = segments.get(up + 1).ok_or_else(|| {
        Status::invalid_argument(format!("write resource name {name:?} is missing the upload uuid"))
    })?;

    // `blobs` must follow the uuid.
    let blobs = segments.get(up + 2);
    if blobs != Some(&"blobs") {
        return Err(Status::invalid_argument(format!(
            "write resource name {name:?} must have 'blobs' after the upload uuid"
        )));
    }

    let hash = segments.get(up + 3).ok_or_else(|| {
        Status::invalid_argument(format!("write resource name {name:?} is missing the hash"))
    })?;
    let size_str = segments.get(up + 4).ok_or_else(|| {
        Status::invalid_argument(format!("write resource name {name:?} is missing the size"))
    })?;

    let digest = Digest::from_hex(hash)
        .map_err(|e| Status::invalid_argument(format!("invalid hash in resource name: {e}")))?;
    let size = parse_size(size_str, name)?;
    let instance = segments[..up].join("/");

    Ok(WriteResource {
        instance,
        uuid: (*uuid).to_string(),
        digest,
        size,
    })
}

/// Parse the `{size}` segment as a non-negative byte count.
fn parse_size(s: &str, name: &str) -> Result<u64, Status> {
    s.parse::<u64>().map_err(|_| {
        Status::invalid_argument(format!("invalid size {s:?} in resource name {name:?}"))
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const HEX: &str = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

    #[test]
    fn read_with_instance() {
        let r = parse_read_resource(&format!("my-instance/blobs/{HEX}/11")).unwrap();
        assert_eq!(r.instance, "my-instance");
        assert_eq!(r.digest.to_hex(), HEX);
        assert_eq!(r.size, 11);
    }

    #[test]
    fn read_empty_instance() {
        let r = parse_read_resource(&format!("blobs/{HEX}/0")).unwrap();
        assert_eq!(r.instance, "");
        assert_eq!(r.size, 0);
    }

    #[test]
    fn read_multi_segment_instance() {
        let r = parse_read_resource(&format!("a/b/c/blobs/{HEX}/5")).unwrap();
        assert_eq!(r.instance, "a/b/c");
    }

    #[test]
    fn read_rejects_missing_blobs() {
        let err = parse_read_resource(&format!("instance/{HEX}/5")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn read_rejects_bad_size() {
        let err = parse_read_resource(&format!("blobs/{HEX}/notanumber")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn read_rejects_compressed() {
        let err =
            parse_read_resource(&format!("instance/compressed-blobs/zstd/{HEX}/5")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unimplemented);
    }

    #[test]
    fn write_full_grammar() {
        let r = parse_write_resource(&format!(
            "my-instance/uploads/550e8400-e29b-41d4-a716-446655440000/blobs/{HEX}/11"
        ))
        .unwrap();
        assert_eq!(r.instance, "my-instance");
        assert_eq!(r.uuid, "550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(r.digest.to_hex(), HEX);
        assert_eq!(r.size, 11);
    }

    #[test]
    fn write_empty_instance_with_trailing_metadata() {
        let r = parse_write_resource(&format!("uploads/the-uuid/blobs/{HEX}/3/extra/metadata"))
            .unwrap();
        assert_eq!(r.instance, "");
        assert_eq!(r.uuid, "the-uuid");
        assert_eq!(r.size, 3);
    }

    #[test]
    fn write_rejects_missing_uploads() {
        let err = parse_write_resource(&format!("blobs/{HEX}/5")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn write_rejects_blobs_not_after_uuid() {
        let err =
            parse_write_resource(&format!("uploads/the-uuid/notblobs/{HEX}/5")).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }
}
