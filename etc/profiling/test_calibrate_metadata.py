import copy
import unittest
from calibrate_metadata import summarize


def sample(strategy, wall, cpu, reads=100):
    return dict(strategy=strategy, result=dict(
        count=dict(entries=1000, dirs=10, bytes=10000, allocated_bytes=12288, errors=0, devices=[1]),
        wall_seconds=wall, user_seconds=cpu / 10, system_seconds=cpu * .9,
        proc_before=dict(disk_read_bytes=0), proc_after=dict(disk_read_bytes=reads)))


class SelectionTests(unittest.TestCase):
    def test_tradeoff_is_relative_to_observed_throughput_not_disk_capacity(self):
        rows = [sample("fast", 1, 8), sample("cheap", 1.2, 3), sample("slow", 2, 1)]
        self.assertEqual(summarize(rows, 1)["selected"], "fast")
        self.assertEqual(summarize(rows, .8)["selected"], "cheap")

    def test_refuses_faster_incomplete_or_incomparable_walks(self):
        rows = [sample("reference", 2, 3), sample("candidate", 1, 1)]
        for field, value in [("errors", 1), ("entries", 999), ("allocated_bytes", 0), ("devices", [2])]:
            broken = copy.deepcopy(rows)
            broken[1]["result"]["count"][field] = value
            with self.assertRaises(ValueError):
                summarize(broken, 1)
        rows[0]["result"]["count"]["devices"] = [1, 2]
        with self.assertRaises(ValueError):
            summarize(rows, 1)

    def test_medians_prevent_one_fast_outlier_from_selecting_slow_strategy(self):
        rows = [sample("steady", t, 4) for t in (2, 2, 2)]
        rows += [sample("noisy", t, 3) for t in (.1, 3, 3)]
        self.assertEqual(summarize(rows, 1)["selected"], "steady")


if __name__ == "__main__":
    unittest.main()
