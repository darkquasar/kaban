package drivers

import "net/http"

// HttpDriver has the method names but not the signatures of DatabaseDriver.
type HttpDriver struct{}

// Open returns a handler, so HttpDriver does not implement DatabaseDriver.
func (h *HttpDriver) Open(dsn string) (http.Handler, error) { return nil, nil }

// Close releases the driver.
func (h *HttpDriver) Close() error { return nil }
