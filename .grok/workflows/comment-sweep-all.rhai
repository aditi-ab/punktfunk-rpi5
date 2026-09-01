let meta = #{
    name: "comment-sweep-all",
    description: "Sweep every first-party crate's comments in one run: parallel rewrite, parallel review, serial commits",
    when_to_use: "Comment sweep across all crates. One workflow run; file rewrites fan out; one git commit per crate at the end.",
    phases: [
        #{ title: "Inventory", detail: "every .rs file in every selected crate" },
        #{ title: "Rewrite", detail: "parallel file batches across crates" },
        #{ title: "Review", detail: "one coverage reviewer per crate" },
        #{ title: "Retry", detail: "rewrite leftovers, re-review failed crates" },
        #{ title: "Commit", detail: "one docs(crate) commit per green crate, serial" },
        #{ title: "Report", detail: "per-crate ledger" },
    ],
};

// args:
//   crates       string[]  optional subset (default: all first-party crates)
//   skip         string[]  crates to leave alone (done or in-flight)
//   dry_run      bool
//   skip_commit  bool
//   review_only  bool      skip rewrite; inventory → review → commit (crash resume)
//   batch_size   int       default 4

let skip = [];
let selected = [];
let dry_run = false;
let skip_commit = false;
let review_only = false;
let batch_size = 4;
if args != () {
    if args.skip != () { skip = args.skip; }
    if args.dry_run != () { dry_run = args.dry_run; }
    if args.skip_commit != () { skip_commit = args.skip_commit; }
    if args.review_only != () { review_only = args.review_only; }
    if args.batch_size != () { batch_size = args.batch_size; }
}
if batch_size < 1 { batch_size = 1; }
if batch_size > 8 { batch_size = 8; }

let all_crates = [
    "pf-bitstream",
    "pf-capture",
    "pf-client-core",
    "pf-clipboard",
    "pf-console-ui",
    "pf-driver-proto",
    "pf-dxvadec",
    "pf-encode",
    "pf-frame",
    "pf-gpu",
    "pf-host-config",
    "pf-inject",
    "pf-paths",
    "pf-presenter",
    "pf-update",
    "pf-update-check",
    "pf-vaadec",
    "pf-vdisplay",
    "pf-vkdecode",
    "pf-win-display",
    "pf-zerocopy",
    "punktfunk-core",
    "punktfunk-encode-worker",
    "punktfunk-host",
    "punktfunk-setup",
    "punktfunk-tray",
];

if args != () && args.crates != () {
    for c in args.crates {
        if crate_name_ok(c) && !list_has(skip, c) { selected.push(c); }
    }
} else {
    for c in all_crates {
        if !list_has(skip, c) { selected.push(c); }
    }
}
if selected.len() == 0 {
    pause("verification", "No crates left after skip. Pass args.crates or a smaller args.skip.");
}

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
    "required": ["files", "skipped", "total_files", "total_blocks"],
    "properties": #{
        "files": #{ "type": "array", "items": file_schema, "maxItems": 600 },
        "skipped": #{ "type": "array", "items": #{ "type": "string" }, "maxItems": 80 },
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
    "required": ["crate", "every_file_ok", "every_comment_ok", "files_total", "files_read", "files_missed", "leftover", "comment_only_diff", "writing_check_ok", "summary"],
    "properties": #{
        "crate": #{ "type": "string" },
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
if review_only {
    log("Inventory " + selected.len().to_string() + " crates (review_only: rewrite already on disk)");
} else {
    log("Inventory " + selected.len().to_string() + " crates (parallel rewrite follows)");
}

let inv = agent(inventory_all_prompt(selected), #{
    label: "inventory:all",
    capability_mode: "execute",
    output_schema: inventory_schema,
});

let files = [];
if inv != () && inv.success && inv.output != () && inv.output.files != () {
    for f in inv.output.files {
        if f != () && path_in_selected(f.path, selected) { files.push(f); }
    }
}
if files.len() == 0 {
    complete(#{
        summary: "No in-crate Rust files (inventory empty or filtered).",
        crates: selected,
        committed: [],
    });
}

let inventory_paths = [];
for f in files { inventory_paths.push(f.path); }
log(files.len().to_string() + " files, " + total_blocks(files).to_string() + " comment blocks");

if dry_run {
    complete(#{
        summary: "Dry run: inventory only.",
        crates: selected,
        files: files.len(),
        total_blocks: total_blocks(files),
        committed: [],
    });
}

// ---------------------------------------------------------------------------
let claimed_paths = [];
if review_only {
    log("Skip rewrite: treating inventory paths as already rewritten");
    for p in inventory_paths { claimed_paths.push(p); }
} else {
    phase("Rewrite");
    let jobs = shard_rewrite_jobs("all", files, batch_size, rewrite_schema);
    log("Rewrite " + jobs.len().to_string() + " batches across " + selected.len().to_string() + " crates");
    let rewrite_results = parallel(jobs);
    for r in rewrite_results {
        if r == () || !r.success || r.output == () { continue; }
        if r.output.files != () {
            for f in r.output.files {
                if f != () && f.path != () { claimed_paths.push(f.path); }
            }
        }
    }
}

// ---------------------------------------------------------------------------
phase("Review");
let review_jobs = [];
let ri = 0;
for c in selected {
    let crate_paths = paths_for_crate(inventory_paths, c);
    if crate_paths.len() == 0 { continue; }
    let unclaimed = [];
    for p in crate_paths {
        if !list_has(claimed_paths, p) { unclaimed.push(p); }
    }
    review_jobs.push(#{
        prompt: coverage_prompt(c, crate_paths, unclaimed),
        label: "review:" + c,
        capability_mode: "execute",
        output_schema: review_schema,
    });
    ri += 1;
}
log("Review " + review_jobs.len().to_string() + " crates in parallel");
let reviews = parallel(review_jobs);

let green = [];
let failed = [];
let leftover_files = [];
let rev_i = 0;
for c in selected {
    let crate_paths = paths_for_crate(inventory_paths, c);
    if crate_paths.len() == 0 { continue; }
    let ok = false;
    let r = ();
    if rev_i < reviews.len() { r = reviews[rev_i]; }
    rev_i += 1;
    if r != () && r.success && r.output != () {
        let files_ok = r.output.every_file_ok == true;
        let comments_ok = r.output.every_comment_ok == true;
        if r.output.comment_only_diff == false { comments_ok = false; }
        if r.output.writing_check_ok == false { comments_ok = false; }
        if r.output.files_missed != () && r.output.files_missed.len() > 0 { files_ok = false; }
        if files_ok && comments_ok {
            green.push(c);
        } else {
            failed.push(c);
            if r.output.files_missed != () {
                for p in r.output.files_missed {
                    if path_in_selected(p, selected) && !list_has(leftover_files, p) {
                        leftover_files.push(p);
                    }
                }
            }
            if r.output.leftover != () {
                for item in r.output.leftover {
                    if item != () && path_in_selected(item.file, selected) && !list_has(leftover_files, item.file) {
                        leftover_files.push(item.file);
                    }
                }
            }
        }
    } else {
        failed.push(c);
        for p in crate_paths {
            if !list_has(leftover_files, p) { leftover_files.push(p); }
        }
    }
}

if failed.len() > 0 {
    phase("Retry");
    if leftover_files.len() == 0 {
        for c in failed {
            for p in paths_for_crate(inventory_paths, c) {
                if !list_has(leftover_files, p) { leftover_files.push(p); }
            }
        }
    }
    log("Retry " + leftover_files.len().to_string() + " files in " + failed.len().to_string() + " crates");
    let retry_jobs = rewrite_jobs_for_paths("retry", leftover_files, 1, rewrite_schema);
    let retry_results = parallel(retry_jobs);
    let _ignore = retry_results;

    let retry_reviews = [];
    for c in failed {
        let crate_paths = paths_for_crate(inventory_paths, c);
        retry_reviews.push(#{
            prompt: coverage_prompt(c, crate_paths, []),
            label: "re-review:" + c,
            capability_mode: "execute",
            output_schema: review_schema,
        });
    }
    let reviews2 = parallel(retry_reviews);
    let failed2 = [];
    let fi = 0;
    for c in failed {
        let r = ();
        if fi < reviews2.len() { r = reviews2[fi]; }
        fi += 1;
        let ok = false;
        if r != () && r.success && r.output != () {
            let files_ok = r.output.every_file_ok == true;
            let comments_ok = r.output.every_comment_ok == true;
            if r.output.comment_only_diff == false { comments_ok = false; }
            if r.output.writing_check_ok == false { comments_ok = false; }
            if r.output.files_missed != () && r.output.files_missed.len() > 0 { files_ok = false; }
            ok = files_ok && comments_ok;
        }
        if ok { green.push(c); } else { failed2.push(c); }
    }
    failed = failed2;
}

// ---------------------------------------------------------------------------
let committed = [];
let commit_notes = [];
if !skip_commit {
    phase("Commit");
    for c in green {
        log("Commit " + c);
        let cres = agent(commit_prompt(c), #{
            label: "commit:" + c,
            capability_mode: "all",
            output_schema: commit_schema,
        });
        let line = c + ": not committed";
        if cres != () && cres.success && cres.output != () && cres.output.committed == true {
            let sha = "";
            if cres.output.sha != () { sha = cres.output.sha; }
            line = c + ": " + sha;
            committed.push(c);
        }
        commit_notes.push(line);
    }
} else {
    commit_notes.push("skip_commit");
}

phase("Report");
let report = "";
report += "# Comment sweep (all)\n\n";
report += "- crates selected: ";
report += selected.len().to_string();
report += "\n- files: ";
report += files.len().to_string();
report += "\n- green: ";
report += green.len().to_string();
report += "\n- failed: ";
report += failed.len().to_string();
report += "\n- committed: ";
report += committed.len().to_string();
report += "\n\n## Commits\n";
for line in commit_notes {
    report += "- ";
    report += line;
    report += "\n";
}
if failed.len() > 0 {
    report += "\n## Failed\n";
    for c in failed {
        report += "- ";
        report += c;
        report += "\n";
    }
}
let report_path = write_scratch_file("comment-sweep-all.md", report);
complete(#{
    path: report_path,
    report: report,
    crates: selected,
    green: green,
    failed: failed,
    committed: committed,
    files: files.len(),
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

fn crate_of(path) {
    if type_of(path) != "string" { return ""; }
    if path.len() < 8 { return ""; }
    if path.sub_string(0, 7) != "crates/" { return ""; }
    let rest = path.sub_string(7, path.len() - 7);
    let i = 0;
    while i < rest.len() {
        if rest.sub_string(i, 1) == "/" { return rest.sub_string(0, i); }
        i += 1;
    }
    ""
}

fn path_in_selected(path, selected) {
    let c = crate_of(path);
    if c == "" { return false; }
    if !list_has(selected, c) { return false; }
    if contains_sub(path, "/vendor/") { return false; }
    if path.len() < 3 { return false; }
    path.sub_string(path.len() - 3, 3) == ".rs"
}

fn paths_for_crate(paths, name) {
    let out = [];
    for p in paths {
        if crate_of(p) == name { out.push(p); }
    }
    out
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
    s += "HOW TO WRITE A COMMENT (docs/writing.md). Read it. Length CI is a backstop, not the style.\n";
    s += "A comment is the live invariant: trap, lifetime, unit on a magic number.\n";
    s += "Present tense. The file as it is. Cover the next five lines with your hand: if the\n";
    s += "comment is only interesting as history, delete it. Keep the rule that is still true.\n";
    s += "Not a poem: no cadence, no punchline, no plot. Name the invariant and stop.\n";
    s += "Not archaeology: old versions, scares, soaks, review tickets, 'this used to' go on the PR.\n";
    s += "A version comment is the live handshake, not a biography of every bump.\n";
    s += "// SAFETY: is a proof, not how the bug was found.\n";
    s += "Module rustdoc: what it is, the contract, how to pin it, where evidence lives.\n";
    s += "Aim 8-20 lines (CI fails at 24). // : at most four (CI fails at six). Do not pad.\n";
    s += "A four-line war story is still wrong. A comment never enforces a trust boundary.\n";
    s += "Comments only. Do not change code, tests, literals, or names.\n";
    s += "Do not add writing-ok unless the extra lines are a SAFETY/lifetime trap.\n";
    s
}

fn inventory_all_prompt(crates) {
    let p = "";
    p += "Inventory EVERY Rust source file in these crates. Use tools. Do not invent paths.\n";
    p += "Crate list (JSON):\n";
    p += json_encode(crates);
    p += "\n\nRun this exact Python and return its JSON.\n\n";
    p += "python3 - <<'PY'\n";
    p += "import json, pathlib, importlib.util, sys\n";
    p += "spec = importlib.util.spec_from_file_location('cw', 'scripts/ci/check-writing.py')\n";
    p += "cw = importlib.util.module_from_spec(spec); spec.loader.exec_module(cw)\n";
    p += "crates = json.loads('''";
    p += json_encode(crates);
    p += "''')\n";
    p += "files=[]; skipped=[]\n";
    p += "for name in crates:\n";
    p += "    root = pathlib.Path('crates')/name\n";
    p += "    if not root.is_dir():\n";
    p += "        skipped.append(name); continue\n";
    p += "    for p in sorted(root.rglob('*.rs')):\n";
    p += "        rel=str(p.as_posix())\n";
    p += "        if '/vendor/' in rel.split('/'):\n";
    p += "            skipped.append(rel); continue\n";
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
    p += "print(json.dumps({'files':files,'skipped':skipped,'total_files':len(files),\n";
    p += "                 'total_blocks':sum(f['comment_blocks'] for f in files)}, indent=2))\n";
    p += "PY\n\n";
    p += "An empty files list is valid ONLY after the Python ran and printed files=[].\n";
    p
}

fn rewrite_prompt(name, paths_json) {
    let p = "";
    p += "Rewrite EVERY comment in these files (batch ";
    p += name;
    p += "). Comments only.\n\n";
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
    p += "7. Set crate='";
    p += name;
    p += "' in the output.\n";
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
    p += " (no .grok, no other crates). Other crates may be dirty; leave them unstaged.\n";
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
