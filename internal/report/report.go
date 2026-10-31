// Package report embeds and fills walk-forward HTML templates (score + price paths).
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

//go:embed window_nm_price_walk_forward_report.html
var priceReportHTML string

//go:embed table_sort.js
var tableSortJS string

func writeFilled(template, scanDir, stem string, report any) error {
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
		out := template
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

// WriteHTML embeds report JSON into the bilingual EN/FA score-path HTML templates.
func WriteHTML(scanDir, stem string, report any) error {
	return writeFilled(reportHTML, scanDir, stem, report)
}

// WritePriceHTML embeds report JSON into the price-path HTML templates.
func WritePriceHTML(scanDir, stem string, report any) error {
	return writeFilled(priceReportHTML, scanDir, stem, report)
}

// FromJSON regenerates HTML for a saved nm_walk_forward*.json or nm_price_walk_forward*.json.
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
	if strings.Contains(stem, "price") {
		return WritePriceHTML(dir, stem, report)
	}
	return WriteHTML(dir, stem, report)
}
