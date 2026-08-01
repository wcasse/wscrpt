// Maintainer workflow: review the wscrpt sticky pad subsystem.
// Run from the wscrpt repo root via Grok Build: /sticky-pad-review
// Not part of the wscrpt binary or crates.io package.

let meta = #{
    name: "sticky-pad-review",
    description: "Parallel review of sticky pad UI, storage, and agent checklist fan-out for iPad/SSH ergonomics",
    when_to_use: "After changing stickies.rs, sticky paint in render.rs, or pad/agent wiring in app.rs",
    phases: [
        #{ title: "Scan", detail: "map sticky pad surface area" },
        #{ title: "Review", detail: "one reviewer per concern" },
        #{ title: "Verify", detail: "adversarial check of findings" },
    ],
};

let findings_schema = #{
    "type": "object",
    "required": ["findings"],
    "properties": #{
        "findings": #{
            "type": "array",
            "maxItems": 8,
            "items": #{
                "type": "object",
                "required": ["file", "issue", "severity"],
                "properties": #{
                    "file": #{ "type": "string" },
                    "issue": #{ "type": "string" },
                    "severity": #{ "type": "string" },
                },
            },
        },
    },
};

let verdict_schema = #{
    "type": "object",
    "required": ["real", "reason", "evidence"],
    "properties": #{
        "real": #{ "type": "boolean" },
        "reason": #{ "type": "string" },
        "evidence": #{ "type": "string" },
    },
};

// Optional path prefix; default is the sticky subsystem hot paths.
let root_arg = if args == () { () } else { args.root };
let root = if root_arg == () { "src" } else { root_arg };

phase("Scan");
let map_prompt = "";
map_prompt += "You are reviewing the wscrpt sticky notepad subsystem in this repo. ";
map_prompt += "Use read_file and grep only (read-only). Inspect these paths if present under ";
map_prompt += root + ": stickies.rs, app.rs sticky_pad handlers, render.rs sticky pad paint, ";
map_prompt += "agent checklist fan-out (Esc w C / apply Y), session layout sticky_pad_visible. ";
map_prompt += "Return at most 6 findings as {file, issue, severity} where severity is low|med|high. ";
map_prompt += "Focus on: focus/key routing (Esc unfocus vs Action layer), save/dirty races, ";
map_prompt += "checklist parse edge cases, and iPad/SSH TUI paint correctness. ";
map_prompt += "Empty findings only after you actually read the code.";

let scan = agent(map_prompt, #{
    label: "sticky-map",
    capability_mode: "read-only",
    output_schema: findings_schema,
});

let seed = [];
if scan != () && scan.success && scan.output.findings != () {
    for f in scan.output.findings {
        seed.push(f);
    }
}
log("scan seeded " + seed.len().to_string() + " findings");

phase("Review");
let dimensions = [
    "focus and key routing for sticky pad on SSH/Blink",
    "Markdown storage save/delete/archive and draft first-save",
    "checklist parse fan-out and human write-back Esc w Y",
    "render paint top-right pad over editor and agent dashboard",
];
let jobs = [];
for d in dimensions {
    let p = "";
    p += "Review the wscrpt sticky pad subsystem for: " + d + ". ";
    p += "Use read_file/grep on src/stickies.rs, sticky-related code in src/app.rs, ";
    p += "src/render.rs paint_sticky_pad, and agent checklist actions. ";
    p += "Do not invent files — only report issues you verified in the tree. ";
    p += "Return at most 6 concrete findings as {file, issue, severity}.";
    jobs.push(#{
        prompt: p,
        label: "review:" + d,
        capability_mode: "read-only",
        output_schema: findings_schema,
    });
}
let results = parallel(jobs);

let findings = [];
for f in seed {
    findings.push(f);
}
for r in results {
    if r != () && r.success && r.output.findings != () {
        for f in r.output.findings {
            findings.push(f);
        }
    }
}

// Cap before verify to keep budget sane (take first 12).
let MAX = 12;
let capped = [];
let fi = 0;
for f in findings {
    if fi < MAX {
        capped.push(f);
    }
    fi += 1;
}
if findings.len() > MAX {
    log("truncating findings from " + findings.len().to_string() + " to " + MAX.to_string());
}
let findings = capped;

if findings.len() == 0 {
    complete(#{
        summary: "No sticky pad findings.",
        confirmed: [],
    });
}

phase("Verify");
let vjobs = [];
for f in findings {
    let vp = "";
    vp += "Adversarially verify this sticky-pad review finding by reading the shipped code: \"";
    vp += f.issue + "\" in " + f.file + ". ";
    vp += "Set real=true only with concrete evidence you independently inspected. ";
    vp += "Otherwise real=false. severity was " + f.severity + ".";
    vjobs.push(#{
        prompt: vp,
        label: "verify:" + f.file,
        capability_mode: "read-only",
        output_schema: verdict_schema,
    });
}
let verdicts = parallel(vjobs);

let confirmed = [];
let i = 0;
for v in verdicts {
    if v != () && v.success && v.output.real == true
        && v.output.evidence != () && v.output.evidence != ""
    {
        confirmed.push(#{
            file: findings[i].file,
            issue: findings[i].issue,
            severity: findings[i].severity,
            evidence: v.output.evidence,
        });
    }
    i += 1;
}

let report = "";
report += "# sticky-pad-review\n\n";
report += "Confirmed " + confirmed.len().to_string() + " / " + findings.len().to_string();
report += " findings after adversarial verify.\n\n";
for c in confirmed {
    report += "- **" + c.severity + "** `" + c.file + "`: " + c.issue + "\n";
    report += "  evidence: " + c.evidence + "\n";
}
let path = write_scratch_file("sticky-pad-review.md", report);
log(confirmed.len().to_string() + " confirmed sticky pad issues");
complete(#{
    summary: confirmed.len().to_string() + " confirmed sticky pad findings",
    path: path,
    confirmed: confirmed,
});
