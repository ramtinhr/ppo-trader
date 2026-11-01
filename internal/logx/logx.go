// Package logx is the shared structured logger (logrus) for the dt product CLI.
package logx

import (
	"os"

	"github.com/sirupsen/logrus"
)

var L = logrus.New()

func init() {
	L.SetOutput(os.Stderr)
	L.SetFormatter(&logrus.TextFormatter{
		FullTimestamp:   true,
		TimestampFormat: "15:04:05",
		DisableColors:   false,
	})
	L.SetLevel(logrus.InfoLevel)
	if os.Getenv("DT_DEBUG") != "" {
		L.SetLevel(logrus.DebugLevel)
	}
}

func Infof(format string, args ...any)  { L.Infof(format, args...) }
func Warnf(format string, args ...any)  { L.Warnf(format, args...) }
func Errorf(format string, args ...any) { L.Errorf(format, args...) }
func Debugf(format string, args ...any) { L.Debugf(format, args...) }
func Fatalf(format string, args ...any) { L.Fatalf(format, args...) }
