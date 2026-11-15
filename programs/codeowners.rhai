// Code owners.
// refs/heads/main moves only to the head of a patch where, for every path the move
// changes, at least one owner of that path approved at exactly that head.
// Owners come from CODEOWNERS (or .github/CODEOWNERS, docs/CODEOWNERS) at the accepted
// head, never the pushed one, so a patch cannot rewrite its own owners.
// Paths no line owns need no approval. Other refs follow the default rule.
// Uses the git host functions, so this program is pinned as engine rhai/2.

fn init(repo) {
    #{ patches: #{} }
}

fn on_op(state, op) {
    if op.type == "xyz.arkiv.patch" && (op.action == "create" || op.action == "revise") {
        let p = state.patches[op.object];
        if type_of(p) != "map" {
            p = #{ author: op.author };
        } else if p.author != op.author {
            return state; // only the patch author may revise
        }
        p.head = op.payload.head;
        p.approvals = #{}; // a new head needs fresh approvals
        state.patches[op.object] = p;
    } else if op.type == "xyz.arkiv.review" && op.action == "approve" {
        let id = op.payload.patch;
        let p = state.patches[id];
        if type_of(p) == "map" && op.payload.head == p.head {
            p.approvals[op.author] = true;
            state.patches[id] = p;
        }
    }
    state
}

fn owners_file(head) {
    for path in ["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"] {
        let text = file_at(head, path);
        if type_of(text) == "string" {
            return text;
        }
    }
    ""
}

fn check_push(state, push) {
    if push.ref != "refs/heads/main" || push.old == "" || push.head == "" {
        return;
    }
    let approvals = ();
    for p in state.patches.values() {
        if p.head == push.head {
            approvals = p.approvals;
        }
    }
    if type_of(approvals) != "map" {
        return `main moves only to a patch head, and ${push.head} is none`;
    }
    let rules = owners_file(push.old);
    for path in changed_paths(push.old, push.head) {
        let owners = codeowners(rules, path);
        if owners.len() > 0 && !owners.some(|o| o in approvals) {
            return `${path} needs an approval from one of its owners at ${push.head}`;
        }
    }
}
