// Transitional: re-export librespot-protocol so downstream crates can migrate
// imports from `librespot_protocol::*` to `spotify_proto::*` incrementally.
// Once all proto files are generated from our own .proto sources, this
// re-export is removed and the generated modules live here directly.
pub use librespot_protocol::*;
