-- ADR-46: reasoning content is a first-class field on persisted messages.
-- Reasoning text (e.g. DeepSeek `reasoning_content`) is captured at the stream
-- boundary and stored alongside the assistant message. The column is NULL for
-- legacy rows and for messages that carried no reasoning; `#[serde(default)]`
-- on `Message.reasoning_content` keeps old JSON blobs decodable regardless.
--
-- Idempotent and additive; a single ALTER into a nullable column needs no
-- table rebuild.

ALTER TABLE messages ADD COLUMN reasoning_content TEXT NULL;