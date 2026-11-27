"""Read Depthcut's AAF export with the reference pyaaf2 library and print
the composition, following each source clip to its file's URL.

    python -m venv .aaf && .aaf/Scripts/pip install pyaaf2
    RCP_AAF_DUMP=<dir> cargo test -p nle-core aaf::tests::writes_mobs_into_the_template
    .aaf/Scripts/python scripts/validate_aaf.py <dir>/rcp_export.aaf

Exits non-zero if pyaaf2 rejects or misreads the file.
"""
import sys, aaf2
with aaf2.open(sys.argv[1], 'r') as f:
    ident = f.header['IdentificationList'][0]
    print('product', ident['ProductName'].value)
    mobs = list(f.content.mobs)
    print('mobs', len(mobs), [type(m).__name__ for m in mobs])
    comps = list(f.content.compositionmobs())
    assert len(comps) == 1
    comp = comps[0]
    print('comp', comp.name, comp.mob_id)
    for slot in comp.slots:
        seg = slot.segment
        print(' slot', slot.slot_id, slot.edit_rate, seg.media_kind, 'len', seg.length)
        for c in seg.components:
            if isinstance(c, aaf2.components.SourceClip):
                mm = c.mob
                src = c.walk()
                chain = list(src)
                last = chain[-1]
                fm = last.mob if last.mob is not None else None
                url = None
                for m in [x.mob for x in chain if x.mob is not None]:
                    d = m.descriptor if isinstance(m, aaf2.mobs.SourceMob) else None
                    if d is not None:
                        url = d['Locator'][0]['URLString'].value
                print('   clip', c.length, 'start', c.start, 'master', mm.name if mm else None, 'slot', c.slot_id, 'url', url)
            else:
                print('   ', type(c).__name__, c.length)
    for m in f.content.mastermobs():
        print('master', m.name, [(s.slot_id, s.segment.media_kind, s.segment.length) for s in m.slots])
    for m in f.content.sourcemobs():
        print('source', m.name, type(m.descriptor).__name__, m.descriptor['Locator'][0]['URLString'].value, [(s.slot_id, s.segment.media_kind) for s in m.slots], m['CreationTime'].value)
print('OK')
