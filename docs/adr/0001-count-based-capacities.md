# ADR-0001: Count-based node capacities in M0

Status: accepted (M0)

Node capacities (F, B, L) are counts of children/messages/entries, not bytes.
M0 is an in-memory correctness milestone: count-based limits make invariant
I5 trivially checkable, and they keep the deliberately tiny test parameters
(F=4, B=8, L=8) meaningful — small counts force deep trees and frequent
flushes/splits regardless of key or value length. Byte-based sizing only
matters once nodes map to fixed-size disk blocks, so it arrives with the disk
layer, which will replace the counts in `Params` with byte budgets and
revisit flush/split thresholds at that point.
