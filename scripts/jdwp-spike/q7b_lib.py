"""Q7b: step INTO from line 100 (super.onNewIntent) - excludes vs none; plus static var-table survey."""
import os, sys, time, subprocess
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import jdwp
from jdwp import *
from spike import adb, fire, loc_str, find_line_locations, SER, PKG, PREFIX, describe_value

def bp_hit(c, loc, tag):
    rid = c.set_request(EV_BREAKPOINT, SUSPEND_THREAD, [m_location(loc)])
    p = fire(tag)
    ev = c.wait_event(lambda e: e["kind"] == EV_BREAKPOINT, timeout=15)
    c.clear_request(EV_BREAKPOINT, rid)
    return ev["thread"], p

