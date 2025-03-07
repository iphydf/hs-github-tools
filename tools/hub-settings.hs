{-# LANGUAGE LambdaCase   #-}
{-# LANGUAGE ViewPatterns #-}
module Main (main) where

import qualified Data.ByteString.Char8 as BS8
import           Data.Maybe            (fromMaybe, listToMaybe)
import qualified Data.Text             as Text
import           Data.Yaml             (decodeFileThrow)
import qualified GitHub
import           GitHub.Tools.Settings (syncSettings, validateSettings)
import           System.Environment    (getArgs, lookupEnv)

main :: IO ()
main = do
    -- Get auth token from the $GITHUB_TOKEN environment variable.
    args <- getArgs
    lookupEnv "GITHUB_TOKEN" >>= \case
        Nothing -> fail "GITHUB_TOKEN environment variable must be set"
        Just (GitHub.OAuth . BS8.pack -> token) ->
            case args of
                orgFile:reposFile:repoFilter -> do
                    orgYaml <- decodeFileThrow orgFile
                    reposYaml <- decodeFileThrow reposFile
                    validateSettings reposYaml
                    syncSettings token orgYaml reposYaml (Text.pack . fromMaybe "" . listToMaybe $ repoFilter)
                _      -> fail "Usage: hub-settings <org.yml> <repos.yml>"
