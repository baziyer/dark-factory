package kernel

import "testing"

func TestAgentAppearanceEncoding(t *testing.T) {
	want := AgentAppearance{Skin: 1, Hair: 2, HairColour: 3, Face: 4, Outfit: 5, ClothesColour: 6, Shoes: 7, Tool: 8, Headwear: 9}
	encoded := encodeAgentAppearance(want)
	got, err := decodeAgentAppearance(encoded)
	if err != nil || got != want {
		t.Fatalf("round trip %q = %+v, %v", encoded, got, err)
	}
	for _, value := range []string{"1/2/3", "01/2/3/4/5/6/7/8/9", "256/2/3/4/5/6/7/8/9"} {
		if _, err := decodeAgentAppearance(value); err == nil {
			t.Fatalf("decoded non-canonical appearance %q", value)
		}
	}
}
