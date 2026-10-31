// Package report embeds and fills the N/M walk-forward HTML template.
package report

import (
	_ "embed"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

//go:embed window_nm_walk_forward_report.html
var reportHTML string

//go:embed table_sort.js
var tableSortJS string

// WriteHTML embeds report JSON into the bilingual EN/FA HTML templates.
func WriteHTML(scanDir, stem string, report any) error {
	payload, err := json.Marshal(report)
	if err != nil {
		return err
	}
	safe := strings.ReplaceAll(string(payload), "</", "<\\/")
	fill := func(lang string) string {
		htmlTag := `<html lang="en" dir="ltr">`
		if lang == "fa" {
			htmlTag = `<html lang="fa" dir="rtl">`
		}
		out := reportHTML
		out = strings.Replace(out, `<html lang="en" dir="ltr">`, htmlTag, 1)
		out = strings.Replace(out, "/*__TABLE_SORT__*/", tableSortJS, 1)
		out = strings.Replace(out, "/*__REPORT__*/null", safe, 1)
		out = strings.Replace(out, "/*__LANG0__*/'en'", fmt.Sprintf("'%s'", lang), 1)
		return out
	}
	base := filepath.Join(scanDir, stem)
	if err := os.WriteFile(base+".html", []byte(fill("en")), 0o644); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(scanDir, stem+"_fa.html"), []byte(fill("fa")), 0o644)
}

// FromJSON regenerates HTML for a saved nm_walk_forward*.json.
func FromJSON(jsonPath string) error {
	b, err := os.ReadFile(jsonPath)
	if err != nil {
		return err
	}
	var report any
	if err := json.Unmarshal(b, &report); err != nil {
		return err
	}
	dir := filepath.Dir(jsonPath)
	stem := strings.TrimSuffix(filepath.Base(jsonPath), ".json")
	return WriteHTML(dir, stem, report)
}
