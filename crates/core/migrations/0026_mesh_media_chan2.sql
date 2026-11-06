-- A completed clip can be visible on more than one channel room (e.g. sent
-- on Bears2, re-flooded by the peer den on Bears). Track one extra channel.
ALTER TABLE mesh_media ADD COLUMN chan_idx2 INTEGER;
