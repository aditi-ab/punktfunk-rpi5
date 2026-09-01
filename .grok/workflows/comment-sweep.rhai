let meta = #{
    name: "comment-sweep",
    description: "Rewrite every comment in one crate to house style; coverage review; one commit",
    when_to_use: "Crate-by-crate comment sweep: every file, every comment, docs(crate) commit. Start small (pf-paths).",
    phases: [
        #{ title: "Inventory", detail: "every .rs file and comment block in the crate" },
        #{ title: "Rewrite", detail: "one agent per file batch; comments only" },
        #{ title: "Review", detail: "independent coverage: every file, every comment" },
        #{ title: "Retry", detail: "rewrite missed files, then re-review" },
        #{ title: "Commit", detail: "one docs(crate) commit if the review is green" },
        #{ title: "Report", detail: "coverage ledger + sha" },
    ],
};

// args:
//   crate        string  required — workspace member under crates/
//   dry_run      bool    inventory only
//   skip_commit  bool    rewrite + review, no git commit
//   batch_size   int     light files per rewrite agent (default 3)

let crate_name = "";
let dry_run = false;
let skip_commit = false;
let batch_size = 3;
if args != () {
    if args.crate != () { crate_name = args.crate; }
    if args.dry_run != () { dry_run = args.dry_run; }
    if args.skip_commit != () { skip_commit = args.skip_commit; }
    if args.batch_size != () { batch_size = args.batch_size; }
}
if crate_name == "" {
    pause("verification", "Pass args.crate — a first-party crate under crates/ (e.g. pf-paths).");
}
if !crate_name_ok(crate_name) {
    pause("verification", "args.crate must be lowercase letters, digits, and hyphens.");
}
if crate_name == "libvpl-sys" || crate_name == "pyrowave-sys" {
    pause("verification", "Vendor sys crates are out of scope. Pass a first-party crate.");
}
if batch_size < 1 { batch_size = 1; }
if batch_size > 8 { batch_size = 8; }

let file_schema = #{
    "type": "object",
    "required": ["path", "lines", "comment_blocks", "comment_lines", "heavy"],
    "properties": #{
        "path": #{ "type": "string" },
        "lines": #{ "type": "integer" },
        "comment_blocks": #{ "type": "integer" },
        "comment_lines": #{ "type": "integer" },
        "over_cap": #{ "type": "integer" },
        "heavy": #{ "type": "boolean" },
    },
};

let inventory_schema = #{
    "type": "object",
    "required": ["crate", "crate_exists", "files", "skipped", "total_files", "total_blocks"],
    "properties": #{
        "crate": #{ "type": "string" },
        "crate_exists": #{ "type": "boolean" },
        "files": #{ "type": "array", "items": file_schema, "maxItems": 220 },
        "skipped": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 40 },
        "total_files": #{ "type": "integer" },
        "total_blocks": #{ "type": "integer" },
        "notes": #{ "type": "string" },
    },
};

let rewrite_file_schema = #{
    "type": "object",
    "required": ["path", "read", "comments_before", "comments_after", "deleted", "shortened", "kept"],
    "properties": #{
        "path": #{ "type": "string" },
        "read": #{ "type": "boolean" },
        "comments_before": #{ "type": "integer" },
        "comments_after": #{ "type": "integer" },
        "deleted": #{ "type": "integer" },
        "shortened": #{ "type": "integer" },
        "kept": #{ "type": "integer" },
        "issues": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 12 },
    },
};

let rewrite_schema = #{
    "type": "object",
    "required": ["assigned_complete", "files"],
    "properties": #{
        "assigned_complete": #{ "type": "boolean" },
        "files": #{ "type": "array", "items": rewrite_file_schema, "maxItems": 12 },
        "code_changed": #{ "type": "boolean" },
        "notes": #{ "type": "string" },
    },
};

let leftover_schema = #{
    "type": "object",
    "required": ["file", "line", "why"],
    "properties": #{
        "file": #{ "type": "string" },
        "line": #{ "type": "integer" },
        "why": #{ "type": "string" },
    },
};

let review_schema = #{
    "type": "object",
    "required": ["every_file_ok", "every_comment_ok", "files_total", "files_read", "files_missed", "leftover", "comment_only_diff", "writing_check_ok", "summary"],
    "properties": #{
        "every_file_ok": #{ "type": "boolean" },
        "every_comment_ok": #{ "type": "boolean" },
        "files_total": #{ "type": "integer" },
        "files_read": #{ "type": "integer" },
        "files_missed": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 80 },
        "leftover": #{ "type": "array", "items": leftover_schema, "maxItems": 40 },
        "comment_only_diff": #{ "type": "boolean" },
        "writing_check_ok": #{ "type": "boolean" },
        "writing_check_errors": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 24 },
        "summary": #{ "type": "string" },
    },
};

let commit_schema = #{
    "type": "object",
    "required": ["committed", "sha", "subject", "summary"],
    "properties": #{
        "committed": #{ "type": "boolean" },
        "sha": #{ "type": "string" },
        "subject": #{ "type": "string" },
        "summary": #{ "type": "string" },
        "files": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 80 },
        "writing_check_ok": #{ "type": "boolean" },
        "blockers": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 12 },
    },
};

// ---------------------------------------------------------------------------
phase("Inventory");
log("Inventory " + crate_name + ": every .rs file, every comment block");

let inv_prompt = inventory_prompt(crate_name);
let inv = agent(inv_prompt, #{
    label: "inventory:" + crate_name,
    capability_mode: "execute",
    output_schema: inventory_schema,
});

let files = [];
if inv != () && inv.success && inv.output != () && inv.output.files != () {
    let raw = inv.output.files;
    for f in raw {
        if f != () && path_in_crate(f.path, crate_name) {
            files.push(f);
        }
    }
}
if files.len() == 0 {
    complete(#{
        summary: "No in-crate Rust files (inventory empty or filtered).",
        crate: crate_name,
        committed: false,
        every_file_ok: false,
        every_comment_ok: false,
    });
}

let inventory_paths = [];
for f in files { inventory_paths.push(f.path); }
log(files.len().to_string() + " files, " + total_blocks(files).to_string() + " comment blocks");

if dry_run {
    complete(#{
        summary: "Dry run: inventory only.",
        crate: crate_name,
        committed: false,
        files: inventory_paths,
        total_files: files.len(),
        total_blocks: total_blocks(files),
    });
}

// ---------------------------------------------------------------------------
phase("Rewrite");
let jobs = shard_rewrite_jobs(crate_name, files, batch_size, rewrite_schema);
log("Rewrite " + jobs.len().to_string() + " batches");
let rewrite_results = parallel(jobs);

let rewrite_failed = false;
let claimed_paths = [];
for r in rewrite_results {
    if r == () || !r.success || r.output == () {
        rewrite_failed = true;
        continue;
    }
    if r.output.assigned_complete != true { rewrite_failed = true; }
    if r.output.code_changed == true { rewrite_failed = true; }
    if r.output.files != () {
        for f in r.output.files {
            if f != () && f.path != () { claimed_paths.push(f.path); }
            if f != () && f.read != true { rewrite_failed = true; }
        }
    }
}

let unclaimed = [];
for p in inventory_paths {
    if !list_has(claimed_paths, p) { unclaimed.push(p); }
}
if unclaimed.len() > 0 { rewrite_failed = true; }

// ---------------------------------------------------------------------------
phase("Review");
let review_prompt = coverage_prompt(crate_name, inventory_paths, unclaimed);
let cov = agent(review_prompt, #{
    label: "review:" + crate_name,
    capability_mode: "execute",
    output_schema: review_schema,
});

let files_ok = false;
let comments_ok = false;
let missed = [];
let leftover = [];
if cov != () && cov.success && cov.output != () {
    files_ok = cov.output.every_file_ok == true;
    comments_ok = cov.output.every_comment_ok == true;
    if cov.output.files_missed != () { missed = cov.output.files_missed; }
    if cov.output.leftover != () { leftover = cov.output.leftover; }
    if cov.output.comment_only_diff == false { comments_ok = false; }
    if cov.output.writing_check_ok == false { comments_ok = false; }
}

if rewrite_failed || !files_ok || !comments_ok || missed.len() > 0 {
    phase("Retry");
    let retry_paths = [];
    for p in unclaimed {
        if path_in_crate(p, crate_name) && !list_has(retry_paths, p) { retry_paths.push(p); }
    }
    for p in missed {
        if path_in_crate(p, crate_name) && !list_has(retry_paths, p) { retry_paths.push(p); }
    }
    for item in leftover {
        if item != () && path_in_crate(item.file, crate_name) && !list_has(retry_paths, item.file) {
            retry_paths.push(item.file);
        }
    }
    if retry_paths.len() == 0 {
        retry_paths = inventory_paths;
    }
    log("Retry " + retry_paths.len().to_string() + " files");
    let retry_jobs = rewrite_jobs_for_paths(crate_name, retry_paths, 1, rewrite_schema);
    let retry_results = parallel(retry_jobs);
    let _ignore = retry_results;

    let cov2_prompt = coverage_prompt(crate_name, inventory_paths, []);
    let cov2 = agent(cov2_prompt, #{
        label: "re-review:" + crate_name,
        capability_mode: "execute",
        output_schema: review_schema,
    });
    files_ok = false;
    comments_ok = false;
    missed = [];
    leftover = [];
    if cov2 != () && cov2.success && cov2.output != () {
        files_ok = cov2.output.every_file_ok == true;
        comments_ok = cov2.output.every_comment_ok == true;
        if cov2.output.files_missed != () { missed = cov2.output.files_missed; }
        if cov2.output.leftover != () { leftover = cov2.output.leftover; }
        if cov2.output.comment_only_diff == false { comments_ok = false; }
        if cov2.output.writing_check_ok == false { comments_ok = false; }
        cov = cov2;
    }
}

if !files_ok || !comments_ok {
    let gate = "Coverage failed for " + crate_name + ". every_file_ok=" + files_ok.to_string();
    gate += " every_comment_ok=" + comments_ok.to_string();
    gate += " missed=" + missed.len().to_string();
    gate += " leftover=" + leftover.len().to_string();
    gate += ". Inspect the crate, then resume to re-review.";
    await_user("verification", gate);

    let cov3 = agent(coverage_prompt(crate_name, inventory_paths, []), #{
        label: "resume-review:" + crate_name,
        capability_mode: "execute",
        output_schema: review_schema,
    });
    files_ok = false;
    comments_ok = false;
    if cov3 != () && cov3.success && cov3.output != () {
        files_ok = cov3.output.every_file_ok == true;
        comments_ok = cov3.output.every_comment_ok == true;
        if cov3.output.files_missed != () { missed = cov3.output.files_missed; }
        if cov3.output.leftover != () { leftover = cov3.output.leftover; }
        if cov3.output.comment_only_diff == false { comments_ok = false; }
        if cov3.output.writing_check_ok == false { comments_ok = false; }
        cov = cov3;
    }
}

// ---------------------------------------------------------------------------
let committed = false;
let sha = "";
let subject = "";
let commit_summary = "not committed";

if files_ok && comments_ok && !skip_commit {
    phase("Commit");
    let cprompt = commit_prompt(crate_name);
    let cres = agent(cprompt, #{
        label: "commit:" + crate_name,
        capability_mode: "all",
        output_schema: commit_schema,
    });
    if cres != () && cres.success && cres.output != () {
        committed = cres.output.committed == true;
        if cres.output.sha != () { sha = cres.output.sha; }
        if cres.output.subject != () { subject = cres.output.subject; }
        if cres.output.summary != () { commit_summary = cres.output.summary; }
        if cres.output.writing_check_ok == false { committed = false; }
    }
} else if skip_commit {
    commit_summary = "skip_commit";
} else {
    commit_summary = "review not green; no commit";
}

phase("Report");
let leftover_n = leftover.len();
let missed_n = missed.len();
let report = "";
report += "# Comment sweep: ";
report += crate_name;
report += "\n\n";
report += "- files: ";
report += files.len().to_string();
report += "\n- comment blocks (inventory): ";
report += total_blocks(files).to_string();
report += "\n- every_file_ok: ";
report += files_ok.to_string();
report += "\n- every_comment_ok: ";
report += comments_ok.to_string();
report += "\n- files_missed: ";
report += missed_n.to_string();
report += "\n- leftover comments: ";
report += leftover_n.to_string();
report += "\n- committed: ";
report += committed.to_string();
report += "\n- sha: ";
report += sha;
report += "\n- subject: ";
report += subject;
report += "\n- commit: ";
report += commit_summary;
report += "\n";
if cov != () && cov.success && cov.output != () && cov.output.summary != () {
    report += "\n## Reviewer\n\n";
    report += cov.output.summary;
    report += "\n";
}
let report_path = write_scratch_file("comment-sweep.md", report);
complete(#{
    path: report_path,
    report: report,
    crate: crate_name,
    committed: committed,
    sha: sha,
    every_file_ok: files_ok,
    every_comment_ok: comments_ok,
    files: files.len(),
    leftover: leftover_n,
    missed: missed_n,
});

// ---------------------------------------------------------------------------
fn crate_name_ok(name) {
    if type_of(name) != "string" { return false; }
    if name.len() == 0 { return false; }
    let i = 0;
    while i < name.len() {
        let ch = name.sub_string(i, 1);
        let ok = false;
        if ch >= "a" && ch <= "z" { ok = true; }
        if ch >= "0" && ch <= "9" { ok = true; }
        if ch == "-" { ok = true; }
        if !ok { return false; }
        i += 1;
    }
    true
}

fn contains_sub(hay, needle) {
    if type_of(hay) != "string" || type_of(needle) != "string" { return false; }
    if needle.len() == 0 { return true; }
    if hay.len() < needle.len() { return false; }
    let i = 0;
    let last = hay.len() - needle.len();
    while i <= last {
        if hay.sub_string(i, needle.len()) == needle { return true; }
        i += 1;
    }
    false
}

fn path_in_crate(path, name) {
    if type_of(path) != "string" { return false; }
    let prefix = "crates/" + name + "/";
    if path.len() < prefix.len() + 3 { return false; }
    if path.sub_string(0, prefix.len()) != prefix { return false; }
    if contains_sub(path, "/vendor/") { return false; }
    path.sub_string(path.len() - 3, 3) == ".rs"
}

fn list_has(xs, item) {
    for x in xs {
        if x == item { return true; }
    }
    false
}

fn total_blocks(files) {
    let n = 0;
    for f in files {
        if f.comment_blocks != () { n += f.comment_blocks; }
    }
    n
}

fn file_is_heavy(f) {
    if f.heavy == true { return true; }
    if f.comment_blocks != () && f.comment_blocks >= 40 { return true; }
    if f.lines != () && f.lines >= 1000 { return true; }
    false
}

fn house_style() {
    let s = "";
    s += "HOUSE STYLE (docs/writing.md; CI scripts/ci/check-writing.py):\n";
    s += "- A comment is the non-local reason: trap, lifetime, unit on a magic number.\n";
    s += "- Names and types are the what. If the next five lines already say it, DELETE.\n";
    s += "- // : at most four lines (CI fails at six). // SAFETY: and FFI/lifetime proofs stay exact.\n";
    s += "- //! / /// module map: what it is, the contract, how to pin it, where evidence lives.\n";
    s += "  Aim 8-20 lines (CI fails at 24). Do not pad a short file up to 8.\n";
    s += "- DELETE: restatement; field reports (dates, devices, OS, log lines, 'Field 2026-');\n";
    s += "  soak minutes; SKUs; milestone codes (WP-C, M3, M7); plan section numbers; review-ticket\n";
    s += "  novels; 'this used to'; ponytail:; self-Q&A; changelog-in-a-comment; metaphor.\n";
    s += "- KEEP: why this number; lifetime / weak-ref / generation; one-line rejected alternative\n";
    s += "  when it is a trap ('do not grant OWNER (OI)(CI)(F) — WRITE_DAC comes back').\n";
    s += "- English. Clear. No plot. A comment never enforces a trust boundary.\n";
    s += "- Comments only. Do not change code, tests, literals, or names.\n";
    s += "- Do not add writing-ok unless the extra lines are a SAFETY/lifetime trap.\n";
    s
}

fn inventory_prompt(name) {
    let p = "";
    p += "Inventory EVERY Rust source file in crate '";
    p += name;
    p += "'. Use tools. Do not invent paths.\n\n";
    p += "1. Confirm crates/";
    p += name;
    p += " exists. If not, crate_exists=false and files=[].\n";
    p += "2. Run this exact Python and use its JSON as the file list:\n\n";
    p += "python3 - <<'PY'\n";
    p += "import json, pathlib, importlib.util\n";
    p += "spec = importlib.util.spec_from_file_location('cw', 'scripts/ci/check-writing.py')\n";
    p += "cw = importlib.util.module_from_spec(spec); spec.loader.exec_module(cw)\n";
    p += "root = pathlib.Path('crates/";
    p += name;
    p += "')\n";
    p += "files=[]\n";
    p += "if root.is_dir():\n";
    p += "    for p in sorted(root.rglob('*.rs')):\n";
    p += "        rel=str(p.as_posix())\n";
    p += "        if '/vendor/' in rel.split('/'):\n";
    p += "            continue\n";
    p += "        lines=p.read_text(encoding='utf-8', errors='replace').splitlines()\n";
    p += "        blocks=list(cw.iter_comment_blocks(lines))\n";
    p += "        over=0; clines=0\n";
    p += "        for kind,s,e in blocks:\n";
    p += "            n=e-s; clines+=n\n";
    p += "            limit=24 if kind=='doc' else 6\n";
    p += "            if n>=limit and not cw._waived(lines,s): over+=1\n";
    p += "        files.append({'path':rel,'lines':len(lines),'comment_blocks':len(blocks),\n";
    p += "                      'comment_lines':clines,'over_cap':over,\n";
    p += "                      'heavy': len(blocks)>=40 or len(lines)>=1000 or clines>=200})\n";
    p += "print(json.dumps({'crate':'";
    p += name;
    p += "','crate_exists':root.is_dir(),'files':files,\n";
    p += "                 'skipped':[],'total_files':len(files),\n";
    p += "                 'total_blocks':sum(f['comment_blocks'] for f in files)}, indent=2))\n";
    p += "PY\n\n";
    p += "3. Return that object. skipped = vendor paths you saw and dropped.\n";
    p += "An empty files list is valid ONLY after the Python ran and printed files=[].\n";
    p
}

fn rewrite_prompt(name, paths_json) {
    let p = "";
    p += "Rewrite EVERY comment in these files of crate '";
    p += name;
    p += "'. Comments only.\n\n";
    p += house_style();
    p += "\nASSIGNED FILES (JSON array). You must read and edit each one:\n";
    p += paths_json;
    p += "\n\nRequired procedure, per file:\n";
    p += "1. read_file the entire file. Count comment blocks before you edit.\n";
    p += "2. Evaluate every //, ///, //!, and /* */ block. Keeping one is a decision, not a skip.\n";
    p += "3. Shorten or delete until house style holds. SAFETY proofs: keep the invariant, drop the story.\n";
    p += "4. Re-read the file. Remaining // < 6 lines, remaining doc < 24, no field reports, no self-talk.\n";
    p += "5. git diff -- the file. If any non-comment line changed, revert the code change.\n";
    p += "Do not rustfmt. Do not commit. Do not touch files you were not assigned.\n";
    p += "assigned_complete=true only if every assigned file was read and every comment evaluated.\n";
    p
}

fn coverage_prompt(name, inventory_paths, unclaimed) {
    let p = "";
    p += "You are the coverage gate for crate '";
    p += name;
    p += "'. Fail closed. Use tools. Do not trust the rewriter.\n\n";
    p += house_style();
    p += "\nInventory paths (JSON):\n";
    p += json_encode(inventory_paths);
    p += "\nUnclaimed after rewrite (JSON):\n";
    p += json_encode(unclaimed);
    p += "\n\nRequired:\n";
    p += "1. Independently `find crates/";
    p += name;
    p += " -name '*.rs' -not -path '*/vendor/*' | sort`.\n";
    p += "   Compare to inventory. Any disk file not in inventory → files_missed.\n";
    p += "2. Read EVERY .rs file in the crate (vendor skipped). files_read = how many you actually read.\n";
    p += "   every_file_ok=true ONLY if files_read equals the independent find AND you read each one.\n";
    p += "3. Parse every comment block. leftover[] = any remaining block that is over cap, a field\n";
    p += "   report, a self-discussion, a recap of the next five lines, or a story. Include file+line+why.\n";
    p += "4. Run: python3 scripts/ci/check-writing.py\n";
    p += "   writing_check_ok = exit 0. Copy errors into writing_check_errors.\n";
    p += "5. git diff HEAD -- crates/";
    p += name;
    p += "  Every changed line must be a comment or a blank inside a comment block.\n";
    p += "   comment_only_diff=false if any code/test/literal changed.\n";
    p += "6. every_comment_ok=true ONLY if leftover is empty AND writing_check_ok AND comment_only_diff\n";
    p += "   AND you evaluated every block (including ones already short).\n";
    p += "An empty files_missed / leftover is valid ONLY after the independent find and the reads.\n";
    p
}

fn commit_prompt(name) {
    let p = "";
    p += "Commit the comment sweep for crate '";
    p += name;
    p += "' as ONE commit. Use tools.\n\n";
    p += "1. git status && git diff --stat HEAD -- crates/";
    p += name;
    p += "\n";
    p += "2. python3 scripts/ci/check-writing.py  — must be exit 0. If not, do not commit.\n";
    p += "3. Confirm the diff is comments-only under crates/";
    p += name;
    p += ". If code changed, do not commit; list blockers.\n";
    p += "4. Stage ONLY crates/";
    p += name;
    p += " (no .grok, no other crates).\n";
    p += "5. Commit with this shape (docs/writing.md):\n";
    p += "   subject: docs(";
    p += name;
    p += "): tighten comments to house style\n";
    p += "   (must be type(scope): summary, <=72 chars, imperative, no period, no 'and', no semicolon)\n";
    p += "   body, wrap 72, under 200 words, why only:\n";
    p += "   Comments in this crate were self-discussion: plan sections, review tickets,\n";
    p += "   and recap of the next lines. House style keeps the trap and the number.\n";
    p += "   No Co-Authored-By.\n";
    p += "6. Return sha (git rev-parse HEAD), the subject, committed=true.\n";
    p += "If there is no diff, committed=false and sha=''.\n";
    p += "Do not push. Do not open a PR.\n";
    p
}

fn shard_rewrite_jobs(name, files, batch, schema) {
    let jobs = [];
    let batch_paths = [];
    let n = 0;
    for f in files {
        if file_is_heavy(f) {
            if batch_paths.len() > 0 {
                jobs.push(rewrite_job(name, batch_paths, n, schema));
                n += 1;
                batch_paths = [];
            }
            jobs.push(rewrite_job(name, [f.path], n, schema));
            n += 1;
        } else {
            batch_paths.push(f.path);
            if batch_paths.len() >= batch {
                jobs.push(rewrite_job(name, batch_paths, n, schema));
                n += 1;
                batch_paths = [];
            }
        }
    }
    if batch_paths.len() > 0 {
        jobs.push(rewrite_job(name, batch_paths, n, schema));
    }
    jobs
}

fn rewrite_jobs_for_paths(name, paths, batch, schema) {
    let jobs = [];
    let batch_paths = [];
    let n = 0;
    for p in paths {
        batch_paths.push(p);
        if batch_paths.len() >= batch {
            jobs.push(rewrite_job(name, batch_paths, n, schema));
            n += 1;
            batch_paths = [];
        }
    }
    if batch_paths.len() > 0 {
        jobs.push(rewrite_job(name, batch_paths, n, schema));
    }
    jobs
}

fn rewrite_job(name, paths, n, schema) {
    #{
        prompt: rewrite_prompt(name, json_encode(paths)),
        label: "rewrite:" + name + ":" + n.to_string(),
        capability_mode: "read-write",
        output_schema: schema,
    }
}
