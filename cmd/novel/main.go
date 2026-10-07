// Command novel downloads and translates Chinese web novels from Qidian and
// SFACG.
package main

import (
	"os"

	"novel/internal/cli"
)

func main() {
	os.Exit(cli.Run(os.Args))
}