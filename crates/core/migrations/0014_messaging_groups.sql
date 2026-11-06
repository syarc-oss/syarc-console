-- Messaging restructure: conversation kinds and group visibility.
ALTER TABLE conversations ADD COLUMN kind TEXT NOT NULL DEFAULT 'dm';
ALTER TABLE conversations ADD COLUMN is_public INTEGER NOT NULL DEFAULT 0;

-- Existing channel-bridge conversations become 'channel' groups.
UPDATE conversations SET kind = 'channel', is_public = 1
WHERE id IN (
    SELECT m.conversation_id FROM mesh_bridge_mappings m
    JOIN mesh_nodes n ON n.id = m.mesh_node_id
    WHERE n.node_id LIKE '#chan%'
);
