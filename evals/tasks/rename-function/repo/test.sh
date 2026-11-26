#!/bin/sh
grep -q compute_total lib.sh && ! grep -q calc_total lib.sh main.sh && [ "$(sh main.sh)" = 6 ]
