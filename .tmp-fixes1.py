import io

p = 'src/platform/limits.rs'
s = io.open(p, encoding='utf-8').read()

# 1. fallback: the shape's reference value, but never a budget this process does not have
old = '''    fn fallback_value(&self, workload: Workload) -> u64 {
        self.default_value(workload, workload.reference_budget())
    }'''
new = '''    fn fallback_value(&self, workload: Workload) -> u64 {
        // The shape's expectation at its reference budget — but a shape can be
        // forced onto a machine smaller than that reference (or onto a build
        // whose ceiling is below it), and then the reference value is a number
        // for a budget this process does not have.
        self.default_value(workload, workload.reference_budget())
    }

    /// The same fallback, bounded by the budget actually in force.
    fn fallback_within(&self, workload: Workload, budget: u64) -> u64 {
        let reference = workload.reference_budget().min(budget);
        self.default_value(workload, reference)
    }'''
assert old in s
s = s.replace(old, new, 1)
old = '''        if configured > upper {
            let effective = spec.fallback_value(self.workload);'''
new = '''        if configured > upper {
            let effective = spec.fallback_within(self.workload, self.budget);'''
assert old in s
s = s.replace(old, new, 1)

# 2. one source for the worst case
start = s.index('    pub fn worst_case_bytes(&self) -> u64 {')
end = s.index('    /// One row per term of [`Self::worst_case_bytes`], for the refusal message.')
s = s[:start] + '''    pub fn worst_case_bytes(&self) -> u64 {
        self.worst_case_terms()
            .into_iter()
            .fold(0u64, |total, (_, bytes)| total.saturating_add(bytes))
    }

''' + s[end:]

# 3. drop the forwards-only helpers
old = '''    fn bytes(&mut self, spec: &KeySpec, configured: Option<u64>) -> u64 {
        self.value(spec, configured)
    }

'''
assert old in s
s = s.replace(old, '', 1)
s = s.replace('derivation.bytes(&', 'derivation.value(&')

# 4. the thread term on Windows covers the loops as well as the connections
old = '''fn threads_for(max_connections: usize, blocking_threads: usize) -> usize {
    if cfg!(windows) {
        max_connections
    } else {
        LOOP_THREADS + blocking_threads
    }
}'''
new = '''fn threads_for(max_connections: usize, blocking_threads: usize) -> usize {
    let loops = LOOP_THREADS + blocking_threads;
    if cfg!(windows) {
        // The HTTP fallback keeps one thread per admitted connection in
        // addition to the two loops and the pool. The connection part is
        // already capped by `max_connections`, so the thread term is that plus
        // the loops.
        loops.saturating_add(max_connections)
    } else {
        loops
    }
}'''
assert old in s
s = s.replace(old, new, 1)

# 5. cores matter for the suggested shape
old = '''/// Whether the shape came from the budget or from the document.'''
new = '''/// Whether the shape came from the budget or from the document.'''
assert old in s

io.open(p, 'w', encoding='utf-8').write(s)

# resources.rs: suggested shape from budget and cores
p = 'src/platform/resources.rs'
s = io.open(p, encoding='utf-8').read()
old = '''    /// The shape a budget suggests. This is only a suggestion: `svc config
    /// tune` writes it into `svcd.conf`, and the running process always follows
    /// the document (user story 21).
    pub fn of_budget(quantized: u64) -> Workload {
        if quantized < 512 * 1024 * 1024 {
            Workload::Tiny
        } else if quantized < 2 * 1024 * 1024 * 1024 {
            Workload::Interactive
        } else {
            Workload::Streaming
        }
    }'''
new = '''    /// The shape a machine suggests: the budget says how much there is, the
    /// core count says whether one core could ever spend it.
    ///
    /// This is only a suggestion: `svc config tune` writes it into
    /// `svcd.conf`, and the running process always follows the document
    /// (user story 21).
    pub fn suggested(quantized: u64, cpus: Option<usize>) -> Workload {
        // A single-core host cannot drive the concurrency a larger shape asks
        // for, whatever its RAM says (user story 1: the shape follows the cores
        // *and* the memory).
        if quantized < 512 * 1024 * 1024 || cpus == Some(1) {
            Workload::Tiny
        } else if quantized < 2 * 1024 * 1024 * 1024 {
            Workload::Interactive
        } else {
            Workload::Streaming
        }
    }

    /// [`Workload::suggested`] for a budget alone. Kept for the shape
    /// thresholds themselves; a started process always passes its core count.
    pub fn of_budget(quantized: u64) -> Workload {
        Workload::suggested(quantized, None)
    }'''
assert old in s
s = s.replace(old, new, 1)
old = '''    /// The budget this shape's shipped numbers were calibrated at. The old
    /// three-level table becomes the expected value *at this budget*.
    pub fn reference_budget(self) -> u64 {'''
new = '''    /// The budget this shape's shipped numbers were calibrated at. The old
    /// three-level table becomes the expected value *at this budget*.
    ///
    /// These are calibration anchors, not the compile-time ceilings: a `low`
    /// build caps the budget at 128 MiB whatever the shape's reference says, and
    /// a value falling back to a reference is bounded by the budget in force.
    pub fn reference_budget(self) -> u64 {'''
assert old in s
s = s.replace(old, new, 1)
io.open(p, 'w', encoding='utf-8').write(s)
print('ok')
