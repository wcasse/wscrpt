// Maintainer workflow: audit sticky note anchors and storage layout.
// Run from the wscrpt repo (or any workspace with .wscrpt/stickies) via Grok Build.
// Read-only by default. Does not rewrite sticky bodies unless args.apply_notes is true
// (still only proposes; agents must not silent-write without human review of the report).

let meta = #{
    name: "sticky-anchor-audit",
    description: "Audit sticky Markdown files for anchor hygiene, task lists, and storage layout",
    when_to_use: "Before release or after sticky pad/anchor changes; pass args.workspace to target a project root",
    phases: [
        #{ title: "Discover", detail: "find sticky files under team/personal paths" },
        #{ title: "Audit", detail: "parallel checks on anchors and task lists" },
        #{ title: "Report", detail: "synthesize stale/risk notes" },
    ],
};

let inventory_schema = #{
    "type": "object",
    "required": ["files"],
    "properties": #{
        "files": #{
            "type": "array",
            "maxItems": 40,
            "items": #{
                "type": "object",
                "required": ["path", "kind"],
                "properties": #{
                    "path": #{ "type": "string" },
                    "kind": #{ "type": "string" },
                },
            },
        },
    },
};

let audit_schema = #{
    "type": "object",
    "required": ["notes"],
    "properties": #{
        "notes": #{
            "type": "array",
            "maxItems": 12,
            "items": #{
                "type": "object",
                "required": ["path", "note", "severity"],
                "properties": #{
                    "path": #{ "type": "string" },
                    "note": #{ "type": "string" },
                    "severity": #{ "type": "string" },
                },
            },
        },
    },
};

let workspace = if args == () { () } else { args.workspace };
if workspace == () {
    // Default: this package (wscrpt) — team stickies under .wscrpt/stickies if any.
    workspace = ".";
}

phase("Discover");
let discover = "";
discover += "Inventory sticky note Markdown files for workspace root: " + workspace + ". ";
discover += "Search for team stickies at <root>/.wscrpt/stickies/*.md and any docs mentioning ";
discover += "personal sticky paths under XDG state. Use list_dir/grep/read_file only. ";
discover += "Return files as {path, kind} where kind is team|personal|docs|unknown. ";
discover += "Cap at 40 paths. Prefer real filesystem paths you observed.";

let inv = agent(discover, #{
    label: "discover-stickies",
    capability_mode: "read-only",
    output_schema: inventory_schema,
});

let files = [];
if inv != () && inv.success && inv.output.files != () {
    for f in inv.output.files {
        // Keep paths that look sticky-related; drop noise.
        let p = f.path;
        let keep = false;
        if p.contains(".wscrpt/stickies") { keep = true; }
        if p.contains("stickies/") { keep = true; }
        if p.contains("stickies.rs") { keep = true; }
        if p.contains("AGENT_NATIVE") { keep = true; }
        if keep {
            files.push(f);
        }
    }
}
log("inventory kept " + files.len().to_string() + " sticky-related paths");

if files.len() == 0 {
    // Still audit the implementation contract when no team stickies exist yet.
    files.push(#{ path: "src/stickies.rs", kind: "docs" });
    files.push(#{ path: "docs/AGENT_NATIVE_ROADMAP.md", kind: "docs" });
}

phase("Audit");
// Shard into at most 4 parallel auditors by packing paths into prompts.
let shards = [[], [], [], []];
let si = 0;
for f in files {
    shards[si % 4].push(f);
    si += 1;
}

let jobs = [];
let shard_i = 0;
for shard in shards {
    if shard.len() == 0 {
        shard_i += 1;
        continue;
    }
    let p = "";
    p += "Audit sticky anchor hygiene and checklist usability for these paths (workspace=";
    p += workspace + "):\n";
    for f in shard {
        p += "- " + f.path + " (" + f.kind + ")\n";
    }
    p += "Read each path. For sticky .md files, check front matter (id, title, anchor_kind, path), ";
    p += "whether file/selection anchors look relative (no absolute /Users paths), ";
    p += "and whether task lists use - [ ] / - [x]. For source/docs, check the documented contract ";
    p += "matches implementation (pad UI, S1 packet attach, S2 checklist Esc w C / Esc w Y). ";
    p += "Return notes as {path, note, severity} severity low|med|high. Max 12 notes.";
    jobs.push(#{
        prompt: p,
        label: "audit-shard-" + shard_i.to_string(),
        capability_mode: "read-only",
        output_schema: audit_schema,
    });
    shard_i += 1;
}

let results = parallel(jobs);
let notes = [];
for r in results {
    if r != () && r.success && r.output.notes != () {
        for n in r.output.notes {
            notes.push(n);
        }
    }
}

phase("Report");
let synth = "";
synth += "Synthesize a short sticky-anchor audit report from these notes. ";
synth += "Group by severity. Do not invent paths. Notes JSON follows:\n";
synth += json_encode(notes);
synth += "\nWrite markdown with sections: Summary, High, Med, Low, Recommended next steps. ";
synth += "Recommend human-only write-back; never claim you modified sticky files.";

let report_agent = agent(synth, #{
    label: "sticky-audit-report",
    capability_mode: "read-only",
});

let report = "# sticky-anchor-audit\n\n";
report += "Workspace: " + workspace + "\n";
report += "Files considered: " + files.len().to_string() + "\n";
report += "Raw notes: " + notes.len().to_string() + "\n\n";
if report_agent != () && report_agent.success {
    report += report_agent.output;
} else {
    report += "Synthesizer failed; dumping raw notes:\n\n";
    for n in notes {
        report += "- **" + n.severity + "** `" + n.path + "`: " + n.note + "\n";
    }
}

let path = write_scratch_file("sticky-anchor-audit.md", report);
complete(#{
    summary: notes.len().to_string() + " sticky audit notes",
    path: path,
    files: files.len(),
    notes: notes.len(),
});
