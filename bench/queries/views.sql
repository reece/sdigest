-- Views over the raw_* tables that `sdbench` loads from bench/ (see connect() in
-- sdbench.py). Every report, notebook and page queries these views, so joins and
-- exclusions are defined once.

-- Every run, minus runs excluded by an annotation.
CREATE OR REPLACE VIEW runs AS
SELECT *
FROM raw_runs
WHERE run_id NOT IN (SELECT run_id FROM raw_annotations WHERE action = 'exclude');

-- Runs that count: sdigest succeeded and nobody else was reading the disk.
CREATE OR REPLACE VIEW good_runs AS
SELECT * FROM runs WHERE exit_status = 0 AND NOT foreign_io;

CREATE OR REPLACE VIEW hosts AS
SELECT id AS host, identity.* FROM raw_hosts;

CREATE OR REPLACE VIEW devices AS
SELECT id AS device, identity.* FROM raw_devices;

CREATE OR REPLACE VIEW datasets AS
SELECT id AS dataset, spec, n_files, total_bytes, size_bytes.median AS median_file_bytes
FROM raw_datasets;

-- Runs with their device and dataset attributes, for ad-hoc slicing.
CREATE OR REPLACE VIEW runs_full AS
SELECT r.*, d.model, d.transport, d.usb_driver, d.link, d.rotational, d.queue_depth,
       d.layers, d.fstype, ds.spec, ds.median_file_bytes
FROM runs r
JOIN devices d USING (device)
JOIN datasets ds USING (dataset);

-- Medians per configuration, with speedup over one thread.
CREATE OR REPLACE VIEW summary AS
WITH g AS (
    SELECT version, host, device, dataset, level, cache, threads,
           count(*) AS n,
           median(elapsed_s) AS med,
           min(elapsed_s) AS lo,
           max(elapsed_s) AS hi,
           median(device_reads_per_s) AS rps,
           median(rate_read_mib_s) AS read_mibs,
           median(rate_effective_mib_s) AS eff_mibs
    FROM good_runs
    GROUP BY ALL
)
SELECT g.version, g.host, g.device, g.dataset, g.level, g.cache, g.threads, g.n,
       round(g.med, 2) AS median_s,
       round(g.lo, 2) AS min_s,
       round(g.hi, 2) AS max_s,
       round(b.med / g.med, 2) AS speedup,
       round(g.rps, 0) AS median_reads_per_s,
       round(g.read_mibs, 1) AS median_read_mib_s,
       round(g.eff_mibs, 1) AS median_effective_mib_s
FROM g
LEFT JOIN g AS b
  ON b.version = g.version AND b.host = g.host AND b.device = g.device
 AND b.dataset = g.dataset AND b.level = g.level AND b.cache = g.cache AND b.threads = 1;

CREATE OR REPLACE VIEW sessions AS
SELECT s.started, r.version, s.host, s.device, s.sweep.dataset AS dataset, s.label,
       count(r.run_id) AS runs, count(r.run_id) FILTER (WHERE r.foreign_io) AS foreign_io,
       s.session_id
FROM raw_sessions s
LEFT JOIN raw_runs r USING (session_id)
GROUP BY ALL;
