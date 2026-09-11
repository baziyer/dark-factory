package kernel

import (
	"fmt"
	"strconv"
	"strings"
)

// AgentAppearance is durable display state. Automatic preserves the stable
// id-derived look; the six bounded slots are interpreted by the web catalogue.
type AgentAppearance struct {
	Automatic     bool
	Skin          uint8
	Hair          uint8
	HairColour    uint8
	Face          uint8
	Outfit        uint8
	ClothesColour uint8
	Shoes         uint8
	Tool          uint8
	Headwear      uint8
}

func encodeAgentAppearance(value AgentAppearance) string {
	if value.Automatic {
		return ""
	}
	return fmt.Sprintf("%d/%d/%d/%d/%d/%d/%d/%d/%d", value.Skin, value.Hair, value.HairColour, value.Face, value.Outfit, value.ClothesColour, value.Shoes, value.Tool, value.Headwear)
}

func decodeAgentAppearance(value string) (AgentAppearance, error) {
	if value == "" {
		return AgentAppearance{Automatic: true}, nil
	}
	parts := strings.Split(value, "/")
	if len(parts) != 9 {
		return AgentAppearance{}, ErrInvalidValue
	}
	values := make([]uint8, len(parts))
	for index, part := range parts {
		parsed, err := strconv.ParseUint(part, 10, 8)
		if err != nil || strconv.FormatUint(parsed, 10) != part {
			return AgentAppearance{}, ErrInvalidValue
		}
		values[index] = uint8(parsed)
	}
	return AgentAppearance{Skin: values[0], Hair: values[1], HairColour: values[2], Face: values[3], Outfit: values[4], ClothesColour: values[5], Shoes: values[6], Tool: values[7], Headwear: values[8]}, nil
}
