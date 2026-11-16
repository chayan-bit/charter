// Sealed patches.
// A commit op pins sha256(key) for a sealed object; a later reveal op
// publishes the key. The reveal counts only if the hash matches.
// Until then the committer owes a release_key obligation.
//
// Proof policy: an issue op may set proof: "required" | "optional" | "none"
// and a spec_hash. A commit naming that issue counts when proof is required
// only with a verified proof (op.proofs, checked by every client) for the
// pinned guest whose public spec_hash is the issue's and whose key_hash is
// the commitment. "optional" records proven: true/false, "none" ignores proofs.

// Policy for issues that do not set `proof`. Edit, then re-pin the program.
fn default_proof() {
    "optional"
}

// Image id of the sealed_patch guest (arkiv_git_zk::image_id_hex()).
fn sealed_patch_image() {
    "326fefc7f5cc5aaa99f2691046dfafe0ff62496174f1eb2b93aaa4840854a0ca"
}

fn init(repo) {
    #{ issues: #{}, sealed: #{}, revealed: #{} }
}

fn on_issue(state, op) {
    if op.action == "create" && !(op.object in state.issues) {
        let p = op.payload;
        let proof = if "proof" in p { p.proof } else { default_proof() };
        let spec = if "spec_hash" in p { p.spec_hash } else { "" };
        state.issues[op.object] = #{ proof: proof, spec_hash: spec };
    }
    state
}

// Exact statement match: `ok` alone would accept a valid proof of another statement.
// `public` is a reserved word in Rhai, so the statement is read as pr["public"].
fn proven(op, issue, commitment) {
    for pr in op.proofs {
        let st = pr["public"];
        if pr.kind == "risc0" && pr.ok && pr.image_id == sealed_patch_image()
            && st.spec_hash == issue.spec_hash && st.key_hash == commitment {
            return true;
        }
    }
    false
}

fn on_commit(state, op) {
    if op.object in state.sealed {
        return state;
    }
    let p = op.payload;
    let s = #{ by: op.author, target: p.object, commitment: p.commitment };
    let issue = if "issue" in p { state.issues[p.issue] } else { () };
    if type_of(issue) == "map" && issue.proof != "none" {
        let ok = proven(op, issue, p.commitment);
        if issue.proof == "required" && !ok {
            print(`commit ${op.id} rejected: issue ${p.issue} requires a matching proof`);
            return state;
        }
        s.issue = p.issue;
        s.proven = ok;
    }
    state.sealed[op.object] = s;
    state
}

fn on_reveal(state, op) {
    let s = state.sealed[op.object];
    if type_of(s) != "map" || op.object in state.revealed {
        return state;
    }
    if sha256_hex(op.payload.key) != s.commitment {
        print(`reveal ${op.id} rejected: key does not match the commitment`);
        return state;
    }
    state.revealed[op.object] = op.payload.key;
    state
}

fn on_op(state, op) {
    if op.type == "xyz.arkiv.issue" {
        return on_issue(state, op);
    }
    if op.type != "xyz.arkiv.sealed" {
        return state;
    }
    switch op.action {
        "commit" => on_commit(state, op),
        "reveal" => on_reveal(state, op),
        _ => state,
    }
}

fn obligations(state) {
    let out = [];
    for id in state.sealed.keys() {
        if !(id in state.revealed) {
            let s = state.sealed[id];
            out.push(#{ kind: "release_key", object: id, target: s.target, by: s.by });
        }
    }
    out
}
