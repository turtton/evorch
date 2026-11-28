//! Approval review, row status and keychain input of the MCP servers dialog.
//!
//! Everything here only reads the model or edits the open draft; approval,
//! revocation, connection tests and secret storage are returned as actions
//! that the model runs off the render thread.
