#!/usr/bin/env python3
"""Count metacomments, directives, system tasks and attributes in the Verilator test corpus.

Original code for this project; reads ../verilator/test_regress/t as data only.
Usage: python3 tools/corpus_survey.py [path-to-test_regress/t]
"""
import re, glob, collections, os
import sys
T=(sys.argv[1] if len(sys.argv)>1 else os.path.join(os.path.dirname(__file__),'..','..','verilator','test_regress','t'))+'/'
srcs=[f for f in glob.glob(T+'*') if re.search(r'\.(v|sv|vh|svh|vlt|vc|pkg)$',f)]
txt={f:open(f,errors='replace').read() for f in srcs}
def top(c,n=200):
    for k,v in c.most_common(n): print(f'{v:6} {k}')
print('sources',len(srcs))
c=collections.Counter()
for t in txt.values():
    for m in re.finditer(r'(?:/\*|//)\s*verilator\s+([a-z_0-9]+)',t): c[m.group(1)]+=1
print('== metacomments'); top(c)
c=collections.Counter()
for t in txt.values():
    for m in re.finditer(r'//\s*(synopsys|synthesis|ambit synthesis|cadence|pragma)\s+([a-z_]+)',t,re.I): c[(m.group(1).lower()+' '+m.group(2).lower())]+=1
print('== pragma comments'); top(c,30)
c=collections.Counter()
for t in txt.values():
    t2=re.sub(r'"(\\.|[^"\\])*"','""',t)
    for m in re.finditer(r'`([A-Za-z_][A-Za-z0-9_]*)',t2): c[m.group(1)]+=1
std='define undef undefineall ifdef ifndef elsif else endif include line timescale resetall default_nettype celldefine endcelldefine unconnected_drive nounconnected_drive begin_keywords end_keywords pragma __FILE__ __LINE__ error uselib protect endprotect verilog systemc_header systemc_header_post systemc_interface systemc_imp_header systemc_implementation systemc_ctor systemc_dtor systemc_class_name verilator_config coverage_block_off accelerate noaccelerate delay_mode_distributed delay_mode_path delay_mode_unit delay_mode_zero portcoerce noportcoerce inline expand_vectornets noexpand_vectornets remove_gatename noremove_gatenames remove_netname noremove_netnames suppress_faults nosuppress_faults enable_portfaults disable_portfaults default_decay_time default_trireg_strength autoexpand_vectornets'.split()
print('== directives (known)'); 
for k in std:
    if c[k]: print(f'{c[k]:6} `{k}')
c=collections.Counter()
for t in txt.values():
    t2=re.sub(r'"(\\.|[^"\\])*"','""',t)
    for m in re.finditer(r'\$([A-Za-z_][A-Za-z0-9_$]*)',t2): c[m.group(1)]+=1
print('== system tf'); top(c,400)
c=collections.Counter()
for t in txt.values():
    for m in re.finditer(r'`begin_keywords\s+"([^"]+)"',t): c[m.group(1)]+=1
print('== begin_keywords'); top(c)
c=collections.Counter()
for t in txt.values():
    for m in re.finditer(r'\(\*\s*([A-Za-z_]+)',t): c[m.group(1)]+=1
print('== attribute instances (* *)'); top(c,30)
