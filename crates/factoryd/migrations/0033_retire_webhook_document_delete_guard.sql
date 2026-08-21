-- Webhook intake was removed from the live product. Historical document
-- snapshots remain readable metadata until their project is explicitly
-- deleted, at which point the Store owns the exact transactional cleanup.
DROP TRIGGER task_documents_immutable_delete;
