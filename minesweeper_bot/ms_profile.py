"""Profile one expert game to find the hotspot."""
import random, sys, os, time, cProfile, pstats, io
sys.path.insert(0, os.path.dirname(__file__))
from ms_client import MSClient
from ms_solver import play_game

c = MSClient(31350)
c.seed(4242)
rng = random.Random(1)
pr = cProfile.Profile()
pr.enable()
res = play_game(c, "expert", {"tiebreak": "info"}, rng)
pr.disable()
print(res)
s = io.StringIO()
pstats.Stats(pr, stream=s).sort_stats("cumulative").print_stats(20)
print(s.getvalue())
c.close()
