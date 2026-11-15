// Bounties with scoped AI agents (docs/p3/agents.md).
// A delegate grants an agent key bounded authority with an xyz.arkiv.agent op;
// every op the agent signs is checked against its grant before the bounty rules see it.
// The bounty rules are bounty.rhai's, plus: an agent patch is paid only up to the
// grant's max_bounty, main moves to an agent patch head only with the configured
// human approval and proof, and only human delegates may approve or record a merge.
//
// The "Agent delegation" section is self-contained: copy it into another program,
// call agent_gate(state, op) at the top of on_op and agent_admin(state, op) for
// xyz.arkiv.agent ops, and use is_human_delegate(state, who) wherever a delegate counts.

// ---- Configuration. Edit, then re-pin the program. ----

// Does main need one human delegate approval (not the patch author) for an agent patch?
fn agent_needs_approval() {
    true
}

// Proof policy for agent patches: "required" | "optional" (records proven) | "none".
fn agent_proof() {
    "optional"
}

// Image id of the sealed_patch guest (arkiv_git_zk::image_id_hex()).
fn sealed_patch_image() {
    "08dd8528d16694707f3baff286a84919e36238f5245f8e2afb971518588713c9"
}

// ---- Agent delegation. ----
// grant  {agent, scopes, max_bounty, until} by a human delegate.
// revoke {agent} by the granter or any human delegate; effective after its order.
// An agent op is accepted only while order <= until, before any revoke, and when the
// scope (the type after "xyz.arkiv.") is granted. Rejections land in state.rejected.

fn agent_init(repo) {
    let d = #{};
    for x in repo.delegates {
        d[x] = true;
    }
    #{ delegates: d, agents: #{}, rejected: #{} }
}

// An agent is never a delegate, even when its key is also listed as one.
fn is_human_delegate(state, who) {
    who in state.delegates && !(who in state.agents)
}

fn scope_of(ty) {
    if ty.starts_with("xyz.arkiv.") { ty.sub_string(10) } else { ty }
}

fn reject(state, op, reason) {
    state.rejected[op.id] = `${op.type} ${op.action} by ${op.author}: ${reason}`;
    state
}

// Returns "" when the op may proceed, else the reason it is ignored.
fn agent_denial(state, op) {
    let g = state.agents[op.author];
    if type_of(g) != "map" {
        return "";
    }
    if "revoked" in g {
        return `grant revoked at order ${g.revoked}`;
    }
    if op.order > g["until"] {
        return `grant expired at order ${g["until"]}`;
    }
    let scope = scope_of(op.type);
    if !(scope in g.scopes) {
        return `scope ${scope} not granted`;
    }
    ""
}

fn valid_grant(p) {
    type_of(p.agent) == "string" && p.agent.len() == 64 && type_of(p.scopes) == "array"
        && type_of(p.max_bounty) == "i64" && p.max_bounty >= 0 && type_of(p["until"]) == "i64"
}

fn agent_admin(state, op) {
    let p = op.payload;
    if op.action == "grant" {
        if !is_human_delegate(state, op.author) {
            return reject(state, op, "only a human delegate may grant");
        }
        if !valid_grant(p) {
            return reject(state, op, "malformed grant");
        }
        state.agents[p.agent] = #{ by: op.author, scopes: p.scopes, max_bounty: p.max_bounty,
            "until": p["until"], granted: op.order };
        return state;
    }
    if op.action == "revoke" {
        let g = state.agents[p.agent];
        if type_of(g) != "map" {
            return reject(state, op, "no grant to revoke");
        }
        if op.author != g.by && !is_human_delegate(state, op.author) {
            return reject(state, op, "only the granter or a human delegate may revoke");
        }
        if !("revoked" in g) {
            g.revoked = op.order;
            state.agents[p.agent] = g;
        }
    }
    state
}

// ---- Bounty rules (bounty.rhai with agent limits). ----

fn init(repo) {
    let s = agent_init(repo);
    s.bounty = #{};
    s.issues = #{};
    s.patches = #{};
    s.heads = #{};
    s.approvals = #{};
    s.merged = #{};
    s.owed = #{};
    s.paid = #{};
    s
}

// Exact statement match against the issue's spec_hash when it sets one.
fn proven(op, spec) {
    for pr in op.proofs {
        let st = pr["public"];
        if pr.kind == "risc0" && pr.ok && pr.image_id == sealed_patch_image()
            && (spec == "" || st.spec_hash == spec) {
            return true;
        }
    }
    false
}

fn on_patch_create(state, op) {
    let p = op.payload;
    let patch = #{ author: op.author, head: p.head, agent: op.author in state.agents };
    if patch.agent {
        let issue = state.issues[p.issue ?? ""] ?? #{};
        let ok = agent_proof() != "none" && proven(op, issue.spec_hash ?? "");
        if agent_proof() == "required" && !ok {
            return reject(state, op, "agent patch needs a matching proof");
        }
        patch.cap = state.agents[op.author].max_bounty;
        patch.proven = ok;
    }
    state.patches[op.object] = patch;
    state.heads[p.head] = op.object;
    state
}

fn on_approve(state, op) {
    let patch = state.patches[op.payload.patch];
    if type_of(patch) != "map" {
        return state;
    }
    if !is_human_delegate(state, op.author) || op.author == patch.author {
        return reject(state, op, "approver is not a human delegate other than the author");
    }
    let who = state.approvals[op.payload.patch] ?? #{};
    who[op.author] = true;
    state.approvals[op.payload.patch] = who;
    state
}

fn on_merged(state, op) {
    let p = op.payload;
    let patch = state.patches[p.patch];
    if type_of(patch) != "map" {
        return state;
    }
    if !is_human_delegate(state, op.author) {
        return reject(state, op, "only a human delegate may record a merge");
    }
    state.merged[patch.head] = #{ to: patch.author, issue: p.closes, patch: p.patch };
    state
}

fn on_payment(state, p) {
    for k in state.owed.keys() {
        let o = state.owed[k];
        if o.to == p.to && o.amount == p.amount {
            state.owed.remove(k);
            state.paid[p.tx] = k;
            break;
        }
    }
    state
}

fn on_op(state, op) {
    let why = agent_denial(state, op);
    if why != "" {
        return reject(state, op, why);
    }
    let p = op.payload;
    switch op.type {
        "xyz.arkiv.agent" => agent_admin(state, op),
        "xyz.arkiv.issue" if op.action == "create" => {
            state.bounty[op.object] = p.bounty ?? 0;
            state.issues[op.object] = #{ spec_hash: p.spec_hash ?? "" };
            state
        }
        "xyz.arkiv.bounty" if op.action == "pledge" && p.issue in state.bounty => {
            state.bounty[p.issue] += p.amount;
            state
        }
        "xyz.arkiv.patch" if op.action == "create" => on_patch_create(state, op),
        "xyz.arkiv.patch" if op.action == "merged" => on_merged(state, op),
        "xyz.arkiv.review" if op.action == "approve" => on_approve(state, op),
        "xyz.arkiv.payment" if !(p.tx in state.paid) => on_payment(state, p),
        _ => state,
    }
}

// main may move to an agent patch head only with the configured approval.
fn check_push(state, push) {
    if push.ref != "refs/heads/main" {
        return;
    }
    let id = state.heads[push.head];
    let patch = state.patches[id ?? ""];
    if type_of(patch) != "map" || !patch.agent {
        return;
    }
    if agent_needs_approval() && (state.approvals[id] ?? #{}).len() == 0 {
        return `agent patch ${id} needs a human delegate approval`;
    }
}

// Runs after an accepted push.
fn on_push(state, push) {
    let m = state.merged[push.head];
    if push.ref != "refs/heads/main" || type_of(m) != "map" {
        return state;
    }
    let amount = state.bounty[m.issue] ?? 0;
    let patch = state.patches[m.patch];
    if patch.agent && amount > patch.cap {
        state.rejected[`bounty:${m.issue}`] =
            `bounty ${amount} exceeds the agent cap ${patch.cap}; not paid`;
    } else if amount > 0 {
        let key = `bounty:${m.issue}`;
        state.owed[m.issue] = #{ kind: "pay", to: m.to, amount: amount,
            reason: `bounty for issue ${m.issue}`, key: key, purpose: key,
            due_order: push.order };
        state.bounty[m.issue] = 0;
    }
    state.merged.remove(push.head);
    state
}

fn obligations(state) {
    state.owed.values()
}
