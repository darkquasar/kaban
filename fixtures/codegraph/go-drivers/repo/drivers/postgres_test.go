package drivers

import "testing"

func TestPostgresOpen(t *testing.T) { (&PostgresDriver{}).Open("x") }
