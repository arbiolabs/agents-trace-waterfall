PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, model TEXT, parent_session_id TEXT, started_at REAL NOT NULL, ended_at REAL, end_reason TEXT,
  input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0, cache_read_tokens INTEGER DEFAULT 0, cache_write_tokens INTEGER DEFAULT 0, cwd TEXT, estimated_cost_usd REAL, actual_cost_usd REAL);
INSERT INTO sessions VALUES('20261023_211601_3deb47','subagent','qwen3:8b','local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b',1792782961.913965,1792783016.2207611,'agent_close',10362,576,9921,0,'/work/conf',0.0,NULL);
INSERT INTO sessions VALUES('local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','oneshot','qwen3:8b',NULL,1792782785.374331,NULL,NULL,31747,901,29956,0,'/work/conf',0.0,NULL);
CREATE TABLE messages (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, role TEXT NOT NULL, content TEXT, tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
  effect_disposition TEXT, timestamp REAL NOT NULL, token_count INTEGER, finish_reason TEXT);
INSERT INTO messages VALUES(20,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','user','(text)',NULL,NULL,NULL,NULL,1792782788.48382,NULL,NULL);
INSERT INTO messages VALUES(21,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','assistant','(text)',NULL,'[{"id": "call_kfdnbiq7", "type": "function", "function": {"name": "read_file", "arguments": "{}"}}]',NULL,NULL,1792782851.2582281,NULL,'tool_calls');
INSERT INTO messages VALUES(22,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','tool','{}','call_kfdnbiq7',NULL,'read_file',NULL,1792782851.853421,NULL,NULL);
INSERT INTO messages VALUES(23,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','assistant','(text)',NULL,'[{"id": "call_f5c7vais", "type": "function", "function": {"name": "terminal", "arguments": "{}"}}]',NULL,NULL,1792782892.6287732,NULL,'tool_calls');
INSERT INTO messages VALUES(24,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','tool','{"error": null, "exit_code": 1}','call_f5c7vais',NULL,'terminal',NULL,1792782892.764395,NULL,NULL);
INSERT INTO messages VALUES(25,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','assistant','(text)',NULL,'[{"id": "call_qm8wow0d", "type": "function", "function": {"name": "delegate_task", "arguments": "{}"}}]',NULL,NULL,1792782961.778996,NULL,'tool_calls');
INSERT INTO messages VALUES(26,'20261023_211601_3deb47','user','(text)',NULL,NULL,NULL,NULL,1792782961.889787,NULL,NULL);
INSERT INTO messages VALUES(27,'20261023_211601_3deb47','assistant','(text)',NULL,'[{"id": "call_hzmvd2rx", "type": "function", "function": {"name": "search_files", "arguments": "{}"}}]',NULL,NULL,1792783000.7813089,NULL,'tool_calls');
INSERT INTO messages VALUES(28,'20261023_211601_3deb47','tool','{}','call_hzmvd2rx',NULL,'search_files',NULL,1792783001.0537531,NULL,NULL);
INSERT INTO messages VALUES(29,'20261023_211601_3deb47','assistant','(text)',NULL,NULL,NULL,NULL,1792783016.0655549,NULL,'stop');
INSERT INTO messages VALUES(30,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','tool','{}','call_qm8wow0d',NULL,'delegate_task',NULL,1792783016.238945,NULL,NULL);
INSERT INTO messages VALUES(31,'local-3ff76e616877a5044b1537c27089e17afd077ede6391c772b0d049e40696cc8b','assistant','(text)',NULL,NULL,NULL,NULL,1792783077.0494201,NULL,'stop');
PRAGMA writable_schema=ON;
CREATE TABLE IF NOT EXISTS sqlite_sequence(name,seq);
DELETE FROM sqlite_sequence;
INSERT INTO sqlite_sequence VALUES('messages',31);
PRAGMA writable_schema=OFF;
COMMIT;
