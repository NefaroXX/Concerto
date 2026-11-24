-- Exact native response parts; NULL preserves legacy transcripts.
ALTER TABLE messages ADD COLUMN gemini_replay TEXT;
