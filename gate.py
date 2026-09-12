"""Already-realized package gate. All payload code runs inside Bubblewrap."""
import argparse
import json
import shlex
from pathlib import Path
from runtime import Session, executable_store


def probe(session, code):
    session.input.write(str(session.python) + " -c " + shlex.quote(code) + "\n")
    return json.loads(session.output.readline())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", type=Path)
    parser.add_argument("--jq", type=Path, help="already realized jq bin output")
    args = parser.parse_args()
    config = json.loads(args.runtime.read_text()) if args.runtime else {}
    jq = args.jq or executable_store("jq")
    assert (jq / "bin/jq").exists(), "realize jq on the host before running this gate"
    identify = '''import os,json
p=os.getppid()
print(json.dumps([p,open(f'/proc/{p}/stat').read().split()[21],os.readlink(f'/proc/{p}/ns/mnt')]))'''
    with Session(**config) as session:
        session.start()
        before = probe(session, identify)
        assert probe(session, f'import os,shutil,json; print(json.dumps([shutil.which("jq"),os.path.exists("{jq}/bin/jq")]))') == [None, False]
        authority = probe(session, '''import ctypes,os,json
libc=ctypes.CDLL(None,use_errno=True)
r=libc.mount(b'none',b'/tmp',b'tmpfs',0,None)
fds=[]
for f in os.listdir('/proc/self/fd'):
 try: fds.append(os.readlink('/proc/self/fd/'+f))
 except OSError: pass
print(json.dumps({'mount':[r,ctypes.get_errno()],'fds':fds,'caps':[l.strip() for l in open('/proc/self/status') if l.startswith('Cap')]}))''')
        assert authority["mount"] == [-1, 1]
        assert all(int(cap.split()[1], 16) == 0 for cap in authority["caps"])
        assert all(path.startswith("pipe:") for path in authority["fds"])
        session.grant("jq", jq)
        session.input.write("jq -nc '{live:true}'\n")
        assert json.loads(session.output.readline()) == {"live": True}
        after = probe(session, identify)
        assert before == after
        print(json.dumps({"gate": "PASS", "before": before, "after": after,
                          "namespaces": session.identity, "authority": authority,
                          "jq": str(jq)}, indent=2))


if __name__ == "__main__":
    main()
