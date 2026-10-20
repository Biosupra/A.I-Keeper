import json,sys,re
d=json.load(open(sys.argv[1],encoding='utf-8'))
comp=d.get("compilation") or {}
print("## name",comp.get("name"),"compiler",comp.get("compilerVersion"),"match",d.get("match"),"deployed",d.get("deployment",{}).get("blockNumber") if isinstance(d.get("deployment"),dict) else "")
abi=d.get("abi") or []
fns=[]
for f in abi:
    if f.get("type")=="function":
        ins=",".join((i.get("type","")+" "+i.get("name","")).strip() for i in f.get("inputs",[]))
        outs=",".join(o.get("type","") for o in f.get("outputs",[]))
        fns.append(f"{f['name']}({ins})->{outs}")
    elif f.get("type")=="constructor":
        print("constructor(",", ".join(i.get("type","")+" "+i.get("name","") for i in f.get("inputs",[])),")")
    elif f.get("type")=="error":
        pass
print("functions",len(fns)); print("; ".join(sorted(fns)))
srcs=d.get("sources") or {}
pat=sys.argv[2] if len(sys.argv)>2 else None
if pat:
    for path,s in srcs.items():
        c=s.get("content","") if isinstance(s,dict) else str(s)
        for m in re.finditer(pat,c):
            st=max(0,m.start()-300); e=min(len(c),m.end()+500)
            print("---- ",path,"match:",m.group(0)); print(c[st:e])
