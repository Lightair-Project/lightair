import re, sys, collections
WAIT = ("mach_msg2_trap","__psynch_cvwait","semaphore_wait_trap","__workq_kernreturn","__semwait_signal","kevent64",
        "start_wqthread","__accept","poll","semaphore_wait_signal_trap","__psynch_mutexwait","swtch_pri","read ","__select","kevent ","__ulock_wait","mach_wait_until","semaphore_timedwait_trap","__recvfrom","__sigsuspend")
lines = open(sys.argv[1]).read().split("\n")
start = next(i for i,l in enumerate(lines) if l.startswith("Call graph:"))
end = next(i for i,l in enumerate(lines) if l.startswith("Total number in stack"))
pat = re.compile(r"^(\s*)(?:[+!:|]\s*)*(\d+)\s+(.*)$")
threads = collections.Counter(); funcs = collections.defaultdict(collections.Counter)
stack = []  # (depth, count, name)
cur = None
def flush(node_list):
    pass
nodes = []
for l in lines[start+1:end]:
    m = re.match(r"^(\s*[+!:| ]*)(\d+)\s+(.*)$", l)
    if not m: continue
    depth = len(m.group(1)); cnt = int(m.group(2)); name = m.group(3)
    nodes.append((depth, cnt, name))
# compute self counts
for i,(d,c,n) in enumerate(nodes):
    if re.match(r"Thread_\d+", n):
        cur = n.split(":",1)[1].strip() if ":" in n else n
        continue
    child = 0; j = i+1
    while j < len(nodes) and nodes[j][0] > d:
        if nodes[j][0] == min(x[0] for x in nodes[i+1:j+1]) and nodes[j][0] > d:
            pass
        j += 1
    # direct children: next-level depth
    kids = [x for x in nodes[i+1:j]]
    if kids:
        kd = min(x[0] for x in kids)
        child = sum(x[1] for x in kids if x[0] == kd)
    self_c = c - child
    if self_c > 0 and not any(n.startswith(w.strip()) for w in WAIT):
        threads[cur] += self_c
        funcs[cur][re.sub(r"\s+\(in .*","",n)[:90]] += self_c
tot = sum(threads.values())
print("busy samples", tot)
for t, c in threads.most_common(14):
    print(f"{c:6d} {t}")
    for f, k in funcs[t].most_common(4):
        print(f"         {k:5d} {f}")
