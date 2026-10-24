//! Small helpers with no download-domain knowledge: run a child process, read JSON, colour a line,
//! stamp a name with the current date, say a byte count out loud. Nothing here knows what a video is — that is the whole point of
//! the directory. Each is a few dozen lines, deliberately: they replace a dependency, not a
//! design.

pub(crate) mod exec;
pub(crate) mod json;
pub(crate) mod size;
pub(crate) mod stamp;
pub(crate) mod style;
