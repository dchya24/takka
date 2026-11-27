import unittest

import cart
import email
import invoice
import report
import utils


class Names(unittest.TestCase):
    def test_new_name_exists_old_is_gone(self):
        self.assertTrue(hasattr(utils, "format_price"))
        self.assertFalse(hasattr(utils, "fmt_price"))

    def test_callers_still_work(self):
        self.assertEqual(cart.cart_line("tea", 450), "tea: $4.50")
        self.assertEqual(invoice.invoice_total(1999), "Total $19.99")
        self.assertEqual(report.report_row("tea", 450), "tea       $4.50")
        self.assertEqual(email.email_subject(5), "Your total is $0.05")


if __name__ == "__main__":
    unittest.main()
