package traefik

import (
	"fmt"
	"net/http"
	"time"
)

type Config struct {
	ComposeScalerURL string  `yaml:"composeScalerUrl"`
	Name             string  `yaml:"name"`
	SessionDuration  string  `yaml:"sessionDuration"`
	DisplayName      string  `yaml:"displayname"`
	Theme            string  `yaml:"theme"`
	RefreshFrequency string  `yaml:"refreshFrequency"`
	CustomCommand    *string `yaml:"customCommand"`
}

func CreateConfig() *Config {
	return &Config{
		ComposeScalerURL: "http://compose-scaler:10000",
		Name:             "",
		SessionDuration:  "",
		DisplayName:      "",
		Theme:            "ghost",
		RefreshFrequency: "5s",
		CustomCommand:    nil,
	}
}

func (c *Config) BuildRequest(middlewareName string) (*http.Request, error) {
	if len(c.ComposeScalerURL) == 0 {
		c.ComposeScalerURL = "http://compose-scaler:10000"
	}

	if len(c.Name) == 0 {
		return nil, fmt.Errorf("You cannot have an empty project name")
	}

	request, err := http.NewRequest("GET", fmt.Sprintf("%s/api/scale", c.ComposeScalerURL), nil)
	if err != nil {
		return nil, err
	}

	q := request.URL.Query()

	if c.SessionDuration != "" {
		_, err = time.ParseDuration(c.SessionDuration)

		if err != nil {
			return nil, fmt.Errorf("error parsing dynamic.sessionDuration: %v", err)
		}

		q.Add("session_duration", c.SessionDuration)
	}

	q.Add("name", c.Name)

	if c.DisplayName != "" {
		q.Add("display_name", c.DisplayName)
	} else {
		// display name defaults as middleware name
		q.Add("display_name", middlewareName)
	}

	if c.Theme == "" {
		c.Theme = "ghost"
	}
	q.Add("theme", c.Theme)

	if c.RefreshFrequency != "" {
		_, err := time.ParseDuration(c.RefreshFrequency)

		if err != nil {
			return nil, fmt.Errorf("error parsing refreshFrequency: %v", err)
		}

		q.Add("refresh_frequency", c.RefreshFrequency)
	}

	if c.CustomCommand != nil {
		q.Add("custom_command", *c.CustomCommand)
	}

	request.URL.RawQuery = q.Encode()
	return request, nil
}
