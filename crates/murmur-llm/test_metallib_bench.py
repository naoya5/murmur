"""Model-free regression checks using the actual Rust cleanup prefix builder."""
import unittest

from metallib_bench import clean_prefix


class CleanupPrefixTests(unittest.TestCase):
    def test_lists_variants_use_complete_production_turns(self):
        on = clean_prefix(lists=True)
        off = clean_prefix(lists=False)
        # Boolean/numeric assertions keep prompt text out of failure logs.
        self.assertEqual(len(on), len(off))
        self.assertGreater(len(on), 1)
        self.assertEqual(len(on) % 2, 1)
        self.assertTrue(on[0]["role"] == off[0]["role"] == "system")
        self.assertTrue(all(message["role"] == "user" for message in on[1::2] + off[1::2]))
        self.assertTrue(all(message["role"] == "assistant" for message in on[2::2] + off[2::2]))
        self.assertTrue(on[1::2] == off[1::2])
        on_rules = set(on[0]["content"].splitlines())
        off_rules = set(off[0]["content"].splitlines())
        self.assertTrue(off_rules < on_rules)
        self.assertEqual(len(on_rules - off_rules), 1)
        self.assertTrue(any("\n- " in message["content"] for message in on[2::2]))
        self.assertTrue(all("\n- " not in message["content"] for message in off[2::2]))


if __name__ == "__main__":
    unittest.main()
