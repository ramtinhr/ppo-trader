package candles

import "testing"

func TestParseWindowMs(t *testing.T) {
	cases := []string{
		"2025-10-02 00:00:00",
		"2025-10-02 00:00",
		"2025-10-02",
		"2025-10-02T00:00:00Z",
	}
	for _, s := range cases {
		ms, err := ParseWindowMs(s)
		if err != nil {
			t.Fatalf("%q: %v", s, err)
		}
		if ms <= 0 {
			t.Fatalf("%q: got %d", s, ms)
		}
	}
}
