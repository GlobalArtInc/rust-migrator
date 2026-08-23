-- migrator:no-transaction
CREATE INDEX CONCURRENTLY IF NOT EXISTS "IDX_widget_name" ON "widget" ("name");
